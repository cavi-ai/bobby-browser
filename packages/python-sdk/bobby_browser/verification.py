"""Evidence strength without changing the command outcome wire contract."""

from typing import Any, Literal, Mapping

VerificationStatus = Literal[
    "notCompleted", "notReported", "conditionObserved", "settled", "validationRejected"
]


def command_verification_status(outcome: Mapping[str, Any]) -> VerificationStatus:
    """Classify recorded evidence, independently of optional post-state reads.

    A condition observation proves only that wait condition. A settled submit
    does not prove server acceptance. This result never authorizes a retry.
    """
    status = outcome.get("status")
    if status not in (
        "completed", "retryableFailure", "needsReconciliation", "policyDenied",
        "resourceExhausted", "restarted", "failed",
    ):
        raise ValueError("unknown command outcome status")
    if status != "completed":
        return "notCompleted"
    evidence = outcome["evidence"]
    settlements = [item.get("outcome") for item in evidence if item.get("kind") == "submitSettlement"]
    if "validationRejected" in settlements:
        return "validationRejected"
    if "settled" in settlements:
        return "settled"
    if any(item.get("kind") == "wait" for item in evidence):
        return "conditionObserved"
    return "notReported"
