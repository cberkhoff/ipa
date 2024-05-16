use std::marker::PhantomData;

use async_trait::async_trait;
use futures::future::try_join;

use crate::{
    error::Error,
    ff::Field,
    protocol::{
        context::UpgradedContext,
        step::{Gate, Step, StepNarrow, TwoHundredFiftySixBitOpStep},
        NoRecord, RecordBinding, RecordId,
    },
    secret_sharing::{
        replicated::{malicious::ExtendableField, semi_honest::AdditiveShare as Replicated},
        Linear as LinearSecretSharing,
    },
};

/// Special context type used for malicious upgrades.
///
/// The `B: RecordBinding` type parameter is used to prevent using the record ID multiple times to
/// implement an upgrade. For example, trying to use the record ID to iterate over both the inner
/// and outer vectors in a `Vec<Vec<T>>` is an error. Instead, one level of iteration can use the
/// record ID and the other can use something like a `BitOpStep`.
///
#[cfg_attr(not(feature = "descriptive-gate"), doc = "```ignore")]
/// ```no_run
/// use ipa_core::protocol::{context::{UpgradeContext, UpgradeToMalicious, UpgradedMaliciousContext as C}, NoRecord, RecordId};
/// use ipa_core::ff::Fp32BitPrime as F;
/// use ipa_core::secret_sharing::replicated::{
///     malicious::AdditiveShare as MaliciousReplicated, semi_honest::AdditiveShare as Replicated,
/// };
/// // Note: Unbound upgrades only work when testing.
/// #[cfg(test)]
/// let _ = <UpgradeContext<C<'_, F>, F, NoRecord> as UpgradeToMalicious<Replicated<F>, _>>::upgrade;
/// let _ = <UpgradeContext<C<'_, F>, F, RecordId> as UpgradeToMalicious<Replicated<F>, _>>::upgrade;
/// #[cfg(test)]
/// let _ = <UpgradeContext<C<'_, F>, F, NoRecord> as UpgradeToMalicious<(Replicated<F>, Replicated<F>), _>>::upgrade;
/// let _ = <UpgradeContext<C<'_, F>, F, NoRecord> as UpgradeToMalicious<Vec<Replicated<F>>, _>>::upgrade;
/// let _ = <UpgradeContext<C<'_, F>, F, NoRecord> as UpgradeToMalicious<(Vec<Replicated<F>>, Vec<Replicated<F>>), _>>::upgrade;
/// ```
///
/// ```compile_fail
/// use ipa_core::protocol::{context::{UpgradeContext, UpgradeToMalicious, UpgradedMaliciousContext as C}, NoRecord, RecordId};
/// use ipa_core::ff::Fp32BitPrime as F;
/// use ipa_core::secret_sharing::replicated::{
///     malicious::AdditiveShare as MaliciousReplicated, semi_honest::AdditiveShare as Replicated,
/// };
/// // This can't be upgraded with a record-bound context because the record ID
/// // is used internally for vector indexing.
/// let _ = <UpgradeContext<C<'_, F>, F, RecordId> as UpgradeToMalicious<Vec<Replicated<F>>, _>>::upgrade;
pub struct UpgradeContext<
    'a,
    C: UpgradedContext<F>,
    F: ExtendableField,
    B: RecordBinding = NoRecord,
> {
    ctx: C,
    record_binding: B,
    _lifetime: PhantomData<&'a F>,
}

impl<'a, C, F, B> UpgradeContext<'a, C, F, B>
where
    C: UpgradedContext<F>,
    F: ExtendableField,
    B: RecordBinding,
{
    pub fn new(ctx: C, record_binding: B) -> Self {
        Self {
            ctx,
            record_binding,
            _lifetime: PhantomData,
        }
    }

    fn narrow<SS: Step>(&self, step: &SS) -> Self
    where
        Gate: StepNarrow<SS>,
    {
        Self::new(self.ctx.narrow(step), self.record_binding)
    }
}

#[async_trait]
pub trait UpgradeToMalicious<'a, T, M>
where
    T: Send,
{
    async fn upgrade(self, input: T) -> Result<M, Error>;
}

#[async_trait]
impl<'a, C, F> UpgradeToMalicious<'a, (), ()> for UpgradeContext<'a, C, F, NoRecord>
where
    C: UpgradedContext<F>,
    F: ExtendableField,
{
    async fn upgrade(self, _input: ()) -> Result<(), Error> {
        Ok(())
    }
}

#[async_trait]
impl<'a, C, F, T, TM, U, UM> UpgradeToMalicious<'a, (T, U), (TM, UM)>
    for UpgradeContext<'a, C, F, NoRecord>
where
    C: UpgradedContext<F>,
    F: ExtendableField,
    T: Send + 'static,
    U: Send + 'static,
    TM: Send + Sized + 'static,
    UM: Send + Sized + 'static,
    for<'u> UpgradeContext<'u, C, F, NoRecord>:
        UpgradeToMalicious<'u, T, TM> + UpgradeToMalicious<'u, U, UM>,
{
    async fn upgrade(self, input: (T, U)) -> Result<(TM, UM), Error> {
        try_join(
            self.narrow(&TwoHundredFiftySixBitOpStep::from(0))
                .upgrade(input.0),
            self.narrow(&TwoHundredFiftySixBitOpStep::from(1))
                .upgrade(input.1),
        )
        .await
    }
}

#[async_trait]
impl<'a, C, F, I, M> UpgradeToMalicious<'a, I, Vec<M>> for UpgradeContext<'a, C, F, NoRecord>
where
    C: UpgradedContext<F>,
    F: ExtendableField,
    I: IntoIterator + Send + 'static,
    I::IntoIter: ExactSizeIterator + Send,
    I::Item: Send + 'static,
    M: Send + 'static,
    for<'u> UpgradeContext<'u, C, F, RecordId>: UpgradeToMalicious<'u, I::Item, M>,
{
    async fn upgrade(self, input: I) -> Result<Vec<M>, Error> {
        let iter = input.into_iter();
        let ctx = self.ctx.set_total_records(iter.len());
        let ctx_ref = &ctx;
        ctx.try_join(iter.enumerate().map(|(i, share)| async move {
            // TODO: make it a bit more ergonomic to call with record id bound
            UpgradeContext::new(ctx_ref.clone(), RecordId::from(i))
                .upgrade(share)
                .await
        }))
        .await
    }
}

#[async_trait]
impl<'a, C, F> UpgradeToMalicious<'a, Replicated<F>, C::Share>
    for UpgradeContext<'a, C, F, RecordId>
where
    C: UpgradedContext<F>,
    F: ExtendableField,
{
    async fn upgrade(self, input: Replicated<F>) -> Result<C::Share, Error> {
        self.ctx.upgrade_one(self.record_binding, input).await
    }
}
pub struct IPAModulusConvertedInputRowWrapper<F: Field, T: LinearSecretSharing<F>> {
    pub timestamp: T,
    pub is_trigger_bit: T,
    pub trigger_value: T,
    _marker: PhantomData<F>,
}

impl<F: Field, T: LinearSecretSharing<F>> IPAModulusConvertedInputRowWrapper<F, T> {
    pub fn new(timestamp: T, is_trigger_bit: T, trigger_value: T) -> Self {
        Self {
            timestamp,
            is_trigger_bit,
            trigger_value,
            _marker: PhantomData,
        }
    }
}

// Impl to upgrade a single `Replicated<F>` using a non-record-bound context. Used for tests.
#[cfg(test)]
#[async_trait]
impl<'a, C, F, M> UpgradeToMalicious<'a, Replicated<F>, M> for UpgradeContext<'a, C, F, NoRecord>
where
    C: UpgradedContext<F>,
    F: ExtendableField,
    M: 'static,
    for<'u> UpgradeContext<'u, C, F, RecordId>: UpgradeToMalicious<'u, Replicated<F>, M>,
{
    async fn upgrade(self, input: Replicated<F>) -> Result<M, Error> {
        let ctx = if self.ctx.total_records().is_specified() {
            self.ctx
        } else {
            self.ctx.set_total_records(1)
        };
        UpgradeContext::new(ctx, RecordId::FIRST)
            .upgrade(input)
            .await
    }
}
