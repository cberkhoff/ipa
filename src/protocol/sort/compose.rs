use crate::secret_sharing::SecretSharing;
use crate::{error::Error, ff::Field, protocol::context::Context};
use embed_doc_image::embed_doc_image;

use super::{apply::apply, shuffle::unshuffle_shares, ComposeStep::UnshuffleRho};

/// This is an implementation of Compose (Algorithm 5) found in the paper:
/// "An Efficient Secure Three-Party Sorting Protocol with an Honest Majority"
/// by K. Chida, K. Hamada, D. Ikarashi, R. Kikuchi, N. Kiribuchi, and B. Pinkas
/// <https://eprint.iacr.org/2019/695.pdf>
/// This protocol composes two permutations by applying one secret-shared permutation(sigma) to another secret-shared permutation(rho)
/// Input: First permutation(sigma) i.e. permutation that sorts all i-1th bits and other permutation(rho) i.e. sort permutation for ith bit
/// Output: All helpers receive secret shares of permutation which sort inputs until ith bits.

#[embed_doc_image("compose", "images/sort/compose.png")]
/// This algorithm composes two permutations (`rho` and `sigma`). Both permutations are secret-shared,
/// and none of the helpers should learn it through this protocol.
/// Steps
/// ![Compose steps][compose]
/// 1. Generate random permutations using prss
/// 2. First permutation (sigma) is shuffled with random permutations
/// 3. Reveal the permutation
/// 4. Revealed permutation is applied locally on another permutation shares (rho)
/// 5. Unshuffle the permutation with the same random permutations used in step 2, to undo the effect of the shuffling
pub async fn compose<F: Field, S: SecretSharing<F>, C: Context<F, Share = S>>(
    ctx: C,
    random_permutations_for_shuffle: (&[u32], &[u32]),
    shuffled_sigma: &[u32],
    mut rho: Vec<S>,
) -> Result<Vec<S>, Error> {
    apply(shuffled_sigma, &mut rho);

    let unshuffled_rho = unshuffle_shares(
        rho,
        random_permutations_for_shuffle,
        ctx.narrow(&UnshuffleRho),
    )
    .await?;

    Ok(unshuffled_rho)
}

#[cfg(all(test, not(feature = "shuttle")))]
mod tests {
    use crate::{
        ff::Fp31,
        protocol::{
            context::Context,
            sort::{
                apply::apply, compose::compose,
                generate_permutation::shuffle_and_reveal_permutation,
            },
            QueryId,
        },
        test_fixture::{generate_shares, join3, Reconstruct, TestWorld},
    };
    use rand::seq::SliceRandom;

    #[tokio::test]
    pub async fn semi_honest() {
        const BATCHSIZE: u32 = 25;
        for _ in 0..10 {
            let mut rng_sigma = rand::thread_rng();
            let mut rng_rho = rand::thread_rng();

            let mut sigma: Vec<u32> = (0..BATCHSIZE).collect();
            sigma.shuffle(&mut rng_sigma);

            let sigma_u128: Vec<u128> = sigma.iter().map(|x| u128::from(*x)).collect();

            let mut rho: Vec<u32> = (0..BATCHSIZE).collect();
            rho.shuffle(&mut rng_rho);
            let rho_u128: Vec<u128> = rho.iter().map(|x| u128::from(*x)).collect();

            let mut rho_composed = rho_u128.clone();
            apply(&sigma, &mut rho_composed);

            let [sigma0, sigma1, sigma2] = generate_shares::<Fp31>(&sigma_u128);

            let [rho0, rho1, rho2] = generate_shares::<Fp31>(&rho_u128);
            let world = TestWorld::new(QueryId);
            let [ctx0, ctx1, ctx2] = world.contexts();

            let sigma_and_randoms = join3(
                shuffle_and_reveal_permutation(ctx0.narrow("shuffle_reveal"), BATCHSIZE, sigma0),
                shuffle_and_reveal_permutation(ctx1.narrow("shuffle_reveal"), BATCHSIZE, sigma1),
                shuffle_and_reveal_permutation(ctx2.narrow("shuffle_reveal"), BATCHSIZE, sigma2),
            )
            .await;

            let h0_future = compose(
                ctx0,
                (
                    sigma_and_randoms[0].1 .0.as_slice(),
                    sigma_and_randoms[0].1 .1.as_slice(),
                ),
                &sigma_and_randoms[0].0,
                rho0,
            );
            let h1_future = compose(
                ctx1,
                (
                    sigma_and_randoms[1].1 .0.as_slice(),
                    sigma_and_randoms[1].1 .1.as_slice(),
                ),
                &sigma_and_randoms[1].0,
                rho1,
            );
            let h2_future = compose(
                ctx2,
                (
                    sigma_and_randoms[2].1 .0.as_slice(),
                    sigma_and_randoms[2].1 .1.as_slice(),
                ),
                &sigma_and_randoms[2].0,
                rho2,
            );

            let result = join3(h0_future, h1_future, h2_future).await;

            // We should get the same result of applying inverse of sigma on rho as in clear
            assert_eq!(&result.reconstruct(), &rho_composed);
        }
    }
}
