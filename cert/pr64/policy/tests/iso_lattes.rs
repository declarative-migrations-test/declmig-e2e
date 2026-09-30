use pr64_policy_cert::{
    select_release_activation, validate_policy_document, ActivationStrategy, FallbackReason,
};
use serde_json::Value;

#[test]
fn iso_lattes_policy_is_valid_and_fails_closed_to_process_generation() {
    let policy: Value = serde_json::from_str(include_str!("../../fixtures/iso-lattes-pr23.json"))
        .expect("ISO Lattes policy must be valid JSON");

    validate_policy_document(&policy, "iso-lattes")
        .expect("ISO Lattes policy must satisfy the shared release policy");

    let decision = select_release_activation(&policy, ActivationStrategy::Auto, true)
        .expect("auto selection must retain process fallback");

    assert_eq!(decision.selected_strategy, ActivationStrategy::ProcessGeneration);
    assert_eq!(
        decision.fallback_reason,
        Some(FallbackReason::InProcessNotSupported)
    );
}
