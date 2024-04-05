use ipa_step_derive::CompactStep;

#[derive(CompactStep)]
pub(crate) enum SaturatedAdditionStep {
    SaturatedAddition,
    IfElse,
}

#[derive(CompactStep)]
pub(crate) enum SaturatedSubtractionStep {
    SaturatedSubtraction,
    MultiplyWithCarry,
}

#[derive(CompactStep)]
pub(crate) enum Fp25519ConversionStep {
    GenerateSecretSharing,
    #[step(child = crate::protocol::boolean::step::BitOpStep)]
    IntegerAddBetweenMasks,
    IntegerAddMaskToX,
    RevealY,
}
