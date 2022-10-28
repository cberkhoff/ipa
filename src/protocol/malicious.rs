use crate::{
    error::{BoxError, Error},
    field::Field,
    helpers::{fabric::Network, Direction},
    protocol::{check_zero::check_zero, context::ProtocolContext, reveal::reveal, RecordId},
    secret_sharing::{MaliciousReplicated, Replicated},
};
use futures::future::try_join;

use serde::{Deserialize, Serialize};

/// A message sent by each helper when they've computed one share of the result
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct UValue<F> {
    payload: F,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Step {
    #[allow(dead_code)]
    ValidateInput,
    #[allow(dead_code)]
    ValidateMultiplySubstep,
    RevealR,
    CheckZero,
}

impl crate::protocol::Step for Step {}

impl AsRef<str> for Step {
    fn as_ref(&self) -> &str {
        match self {
            Self::ValidateInput => "validate_input",
            Self::ValidateMultiplySubstep => "validate_multiply",
            Self::RevealR => "reveal_r",
            Self::CheckZero => "check_zero",
        }
    }
}

/// This code is an implementation of the approach found in the paper:
/// "Fast Large-Scale Honest-Majority MPC for Malicious Adversaries"
/// by K. Chida, D. Genkin, K. Hamada, D. Ikarashi, R. Kikuchi, Y. Lindell, and A. Nof
/// <https://link.springer.com/content/pdf/10.1007/978-3-319-96878-0_2.pdf>
///
/// As the paragraph labeled "Reducing Memory" on page 25 explains very well, it's more efficient
/// to utilize protocol 5.3 as compared to Protocol 4.1.
/// As that paragraph explains:
/// "...the parties can locally store the partial sums for `u_i` and `w_i`,
/// and all previous shares that are no longer needed for the circuit evaluation can be discarded."
///
/// For this reason, this implementation follows Protocol 5.3: "Computing Arithmetic Circuits Over Any Finite F"
///
/// The summary of the protocol is as follows:
/// 1.) The parties utilize shared randomness to generate (without interaction) a secret-sharing of an unknown value `r`
/// 2.) The parties multiply their secret-sharings of each input to the arithmetic circuit by `r` to obtain a "mirror" of each input share
/// 3.) For all local operations (i.e. addition, subtraction, negation, multiplication by a constant), the parties locally perform those
/// operations on both the sharing of the original value, and the sharing of the original value times r
/// 4.) Each time that there is a protocol involving communication between the helpers, which is secure **up to an additive attack**,
/// the parties perform the protocol in duplicate, once to obtain the originally intended output and once to obtain that output times `r`.
/// For example, instead of just multiplying `a` and `b`, the parties now hold sharings of (`a`, `r*a`) and (`b`, `r*b`).
/// They perform two multiplication protocols to obtain sharings of both (`a*b`, `r*a*b`).
/// 5.) For each input, and for each multiplication or reshare, (basically any time one of the parties had an opportunity to launch an additive attack)
/// we update two information-theoretic MACs. Each MAC is a dot-product.
/// `[u] = Σ[α_k][r*z_k]`
/// `[w] = Σ[αk][zk]`
/// where `z_k` represents every original input, or output of a multiplication,
/// and where `r*z_k` is just the mirror value (the original times `r`) that is being computed along the way through the circuit.
/// The `α_k` are randomly secret-shared values which the parties can generate without interaction using PRSS.
/// Clearly, the only difference between `[u]` and `[w]` is a factor of `r`.
/// 6.) Once the arithmetic circuit is complete, the parties can reveal the randomly chosen value `r`.
/// 7.) Now the parties can each locally compute `[T] = [u] - r*[w]`
/// 8.) Finally, the parties can run the `CheckZero` protocol to confirm that `[T]` is a sharing of zero.
/// If it is NOT, this indicates that one of the parties must have at some point launched an additive attack, and the parties should abort the protocol.
///
/// The really nice thing, is that computing the dot-product of two secret shared vectors can be done at the cost of just one multiplication.
/// This means that we can locally accumulate values along the way, and only perform a tiny amount of communication when the arithmetic circuit is complete
/// and the parties wish to validate the circuit. This makes for a very memory efficient implementation.
///
#[allow(dead_code)]
pub struct SecurityValidator<F> {
    r_share: Replicated<F>,
    u: F,
    w: F,
}

impl<F: Field> SecurityValidator<F> {
    #[must_use]
    #[allow(clippy::needless_pass_by_value)]
    pub fn new<N: Network>(ctx: ProtocolContext<'_, N>) -> SecurityValidator<F> {
        let prss = ctx.prss();

        let r_share = prss.generate_replicated(RecordId::from(0));
        let (u_left, u_right): (F, F) = prss.generate_fields(RecordId::from(1));
        let (w_left, w_right): (F, F) = prss.generate_fields(RecordId::from(2));

        SecurityValidator {
            r_share,
            u: u_right - u_left,
            w: w_right - w_left,
        }
    }

    pub fn r_share(&self) -> Replicated<F> {
        self.r_share
    }

    fn compute_dot_product_contribution(a: Replicated<F>, b: Replicated<F>) -> F {
        (a.left() + a.right()) * (b.left() + b.right()) - a.right() * b.right()
    }

    #[allow(clippy::needless_pass_by_value)]
    pub fn accumulate_macs<N: Network>(
        &mut self,
        ctx: ProtocolContext<'_, N>,
        record_id: RecordId,
        input: MaliciousReplicated<F>,
    ) {
        // The helpers need to use the same shared randomness to generate the random constant used to validate a given multiplication.
        // This is a bit tricky, because we cannot count on the multiplications being executed in the same order across all the helpers.
        // The easiest way is to just narrow the context used to perform the multiplication, and then re-use the same record_id.
        // This ensures that when the helpers all go to validate the multiplication: "1/foo/bar/baz", they all use the prss from "1/foo/bar/baz/validate".
        // That way, we don't need to worry about the order in which the multiplications are executed.
        let random_constant = ctx.prss().generate_replicated(record_id);

        self.u += Self::compute_dot_product_contribution(random_constant, input.rx());
        self.w += Self::compute_dot_product_contribution(random_constant, input.x());
    }

    /// ## Errors
    /// If the two information theoretic MACs are not equal (after multiplying by `r`), this indicates that one of the parties
    /// must have launched an additive attack. At this point the honest parties should abort the protocol. This method throws an
    /// error in such a case.
    pub async fn validate<N: Network>(&self, ctx: ProtocolContext<'_, N>) -> Result<(), BoxError> {
        let record_0 = RecordId::from(0);
        let record_1 = RecordId::from(1);
        let record_2 = RecordId::from(2);
        let record_3 = RecordId::from(3);

        // send our `u_i+1` value to the helper on the right
        let channel = ctx.mesh();
        let helper_right = ctx.role().peer(Direction::Right);
        let helper_left = ctx.role().peer(Direction::Left);
        try_join(
            channel.send(helper_right, record_0, UValue { payload: self.u }),
            channel.send(helper_right, record_1, UValue { payload: self.w }),
        )
        .await?;

        // receive `u_i` value from helper to the left
        let (u_left_struct, w_left_struct): (UValue<F>, UValue<F>) = try_join(
            channel.receive(helper_left, record_0),
            channel.receive(helper_left, record_1),
        )
        .await?;

        let u_left = u_left_struct.payload;
        let w_left = w_left_struct.payload;

        let u_share = Replicated::new(u_left, self.u);
        let w_share = Replicated::new(w_left, self.w);

        // This should probably be done in parallel with the futures above
        let r = reveal(ctx.narrow(&Step::RevealR), record_2, self.r_share).await?;
        let t = u_share - (w_share * r);

        let is_valid = check_zero(ctx.narrow(&Step::CheckZero), record_3, t).await?;

        if is_valid {
            Ok(())
        } else {
            Err(Box::new(Error::MaliciousSecurityCheckFailed))
        }
    }
}

#[cfg(test)]
pub mod tests {
    use crate::error::BoxError;
    use crate::field::Fp31;
    use crate::protocol::{
        malicious::{SecurityValidator, Step},
        QueryId, RecordId,
    };
    use crate::secret_sharing::MaliciousReplicated;
    use crate::test_fixture::{logging, make_contexts, make_world, share, TestWorld};
    use futures::future::{try_join, try_join_all};
    use proptest::prelude::Rng;

    /// This is the simplest arithmetic circuit that allows us to test all of the pieces of this validator
    /// A -
    ///     \
    ///      Mult_Gate -> A*B
    ///     /
    /// B -
    ///
    /// This circuit has two inputs, A and B. These two inputs are multiplied together. That's it.
    ///
    /// To acheive malicious security, the entire circuit must be run twice, once with the original inputs,
    /// and once with all the inputs times a random, secret-shared value `r`. Two information theoretic MACs
    /// are updated; once for each input, and once for each multiplication. At the end of the circuit, these
    /// MACs are compared. If any helper deviated from the protocol, chances are that the MACs will not match up.
    /// There is a small chance of failure which is `2 / |F|`, where `|F|` is the cardinality of the prime field.
    #[tokio::test]
    async fn test_simplest_circuit() -> Result<(), BoxError> {
        logging::setup();

        let world: TestWorld = make_world(QueryId);
        let context = make_contexts(&world);
        let mut rng = rand::thread_rng();

        let a = Fp31::from(rng.gen::<u128>());
        let b = Fp31::from(rng.gen::<u128>());

        let a_shares = share(a, &mut rng);
        let b_shares = share(b, &mut rng);

        let futures = (0..3).into_iter().zip(context).map(|(i, ctx)| async move {
            let mut v = SecurityValidator::new(ctx.narrow(&"SecurityValidatorInit".to_string()));
            let r_share = v.r_share();

            let a_ctx = ctx.narrow("multiply");
            let b_ctx = ctx.clone();

            let (ra, rb) = try_join(
                a_ctx
                    .multiply(RecordId::from(0))
                    .await
                    .execute(a_shares[i], r_share),
                b_ctx
                    .multiply(RecordId::from(1))
                    .await
                    .execute(b_shares[i], r_share),
            )
            .await?;

            let a_malicious = MaliciousReplicated::new(a_shares[i], ra);
            let b_malicious = MaliciousReplicated::new(b_shares[i], rb);

            v.accumulate_macs(
                a_ctx.narrow(&Step::ValidateInput),
                RecordId::from(0),
                a_malicious,
            );
            v.accumulate_macs(
                b_ctx.narrow(&Step::ValidateInput),
                RecordId::from(1),
                b_malicious,
            );

            let (ab, rab) = try_join(
                a_ctx
                    .narrow(&"SingleMult".to_string())
                    .multiply(RecordId::from(0))
                    .await
                    .execute(a_shares[i], b_shares[i]),
                a_ctx
                    .narrow(&"DoubleMult".to_string())
                    .multiply(RecordId::from(1))
                    .await
                    .execute(ra, b_shares[i]),
            )
            .await?;

            v.accumulate_macs(
                a_ctx.narrow(&Step::ValidateMultiplySubstep),
                RecordId::from(0),
                MaliciousReplicated::new(ab, rab),
            );

            v.validate(ctx.narrow(&"SecurityValidatorValidate".to_string()))
                .await
        });

        try_join_all(futures).await?;

        Ok(())
    }

    /// This is a big more complex arithmetic circuit that tests the validator a bit more thoroughly
    /// input1   -
    ///              input1 * input2
    /// input2   -
    ///              input2 * input3
    /// input3   -
    /// ...
    /// input98  -
    ///              input98 * input99
    /// input99  -
    ///              input99 * input100
    /// input100 -
    ///
    /// This circuit has 100 inputs. Each input is multiplied with the adjacent inputs to produce 99 outputs.
    ///
    /// To acheive malicious security, the entire circuit must be run twice, once with the original inputs,
    /// and once with all the inputs times a random, secret-shared value `r`. Two information theoretic MACs
    /// are updated; once for each input, and once for each multiplication. At the end of the circuit, these
    /// MACs are compared. If any helper deviated from the protocol, chances are that the MACs will not match up.
    /// There is a small chance of failure which is `2 / |F|`, where `|F|` is the cardinality of the prime field.
    #[tokio::test]
    async fn test_complex_circuit() -> Result<(), BoxError> {
        logging::setup();

        let world: TestWorld = make_world(QueryId);
        let context = make_contexts(&world);
        let mut rng = rand::thread_rng();

        let mut shared_inputs = [
            Vec::with_capacity(100),
            Vec::with_capacity(100),
            Vec::with_capacity(100),
        ];
        for _ in 0..100 {
            let x = Fp31::from(rng.gen::<u128>());
            let x_shared = share(x, &mut rng);
            shared_inputs[0].push(x_shared[0]);
            shared_inputs[1].push(x_shared[1]);
            shared_inputs[2].push(x_shared[2]);
        }

        let futures =
            context
                .into_iter()
                .zip(shared_inputs)
                .map(|(ctx, input_shares)| async move {
                    let mut v =
                        SecurityValidator::new(ctx.narrow(&"SecurityValidatorInit".to_string()));
                    let r_share = v.r_share();

                    let mut inputs = Vec::with_capacity(100);
                    for i in 0..100 {
                        let step = format!("record_{}_hack", i);
                        let record_id = RecordId::from(i);
                        let record_narrowed_ctx = ctx.narrow(&step);

                        let x = input_shares[usize::try_from(i).unwrap()];
                        inputs.push((record_narrowed_ctx, record_id, x));
                    }

                    let rx_values = try_join_all(inputs.iter().map(
                        |(record_narrowed_ctx, record_id, x)| async move {
                            record_narrowed_ctx
                                .multiply(*record_id)
                                .await
                                .execute(*x, r_share)
                                .await
                        },
                    ))
                    .await?;

                    for i in 0..100 {
                        let (narrowed_ctx, record_id, x) = &inputs[i];
                        let rx = &rx_values[i];
                        v.accumulate_macs(
                            narrowed_ctx.narrow(&Step::ValidateInput),
                            *record_id,
                            MaliciousReplicated::new(*x, *rx),
                        );
                    }

                    let mut mult_inputs = Vec::with_capacity(99);
                    for i in 0..99 {
                        let (narrowed_ctx, record_id, a) = &inputs[i];
                        let (_, _, b) = &inputs[i + 1];
                        let rb = &rx_values[i + 1];

                        mult_inputs.push((narrowed_ctx, *record_id, *a, *b, *rb));
                    }

                    #[allow(clippy::similar_names)]
                    let (ab_outputs, double_check_outputs) = try_join(
                        try_join_all(mult_inputs.iter().map(
                            |(narrowed_ctx, record_id, a, b, _)| async move {
                                narrowed_ctx
                                    .narrow(&"SingleMult".to_string())
                                    .multiply(*record_id)
                                    .await
                                    .execute(*a, *b)
                                    .await
                            },
                        )),
                        try_join_all(mult_inputs.iter().map(
                            |(narrowed_ctx, record_id, a, _, rb)| async move {
                                narrowed_ctx
                                    .narrow(&"DoubleMult".to_string())
                                    .multiply(*record_id)
                                    .await
                                    .execute(*a, *rb)
                                    .await
                            },
                        )),
                    )
                    .await?;

                    for i in 0..99 {
                        let ab = ab_outputs[i];
                        let rab = double_check_outputs[i];
                        let (narrowed_ctx, record_id, _, _, _) = &mult_inputs[i];
                        v.accumulate_macs(
                            narrowed_ctx.narrow(&Step::ValidateMultiplySubstep),
                            *record_id,
                            MaliciousReplicated::new(ab, rab),
                        );
                    }

                    v.validate(ctx.narrow(&"SecurityValidatorValidate".to_string()))
                        .await
                });

        try_join_all(futures).await?;

        Ok(())
    }
}