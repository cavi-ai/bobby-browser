use network_engine::{DestinationPolicy, NetworkPolicy};

async fn validate(host: &str, policy: NetworkPolicy) -> bool {
    DestinationPolicy::new(policy)
        .resolve_and_validate(&format!("http://{host}/"))
        .await
        .is_ok()
}

#[tokio::test]
async fn denies_non_public_destinations_by_default() {
    for host in [
        "127.0.0.1",
        "10.0.0.1",
        "172.16.0.1",
        "192.168.0.1",
        "169.254.10.20",
        "169.254.169.254",
        "[::1]",
        "[fc00::1]",
        "[fe80::1]",
        "0.0.0.0",
        "[::]",
        "224.0.0.1",
        "[ff02::1]",
    ] {
        assert!(!validate(host, NetworkPolicy::default()).await, "{host}");
    }
}

#[tokio::test]
async fn allows_public_literal_destinations() {
    let validated = DestinationPolicy::new(NetworkPolicy::default())
        .resolve_and_validate("http://93.184.216.34/")
        .await
        .unwrap();
    assert_eq!(validated.url().as_str(), "http://93.184.216.34/");
    assert_eq!(validated.addresses().len(), 1);
    assert!(validate("[2001:4860:4860::8888]", NetworkPolicy::default()).await);
}

#[tokio::test]
async fn loopback_exception_does_not_allow_other_private_ranges() {
    let policy = NetworkPolicy {
        allow_loopback: true,
        ..NetworkPolicy::default()
    };

    assert!(validate("127.0.0.1", policy.clone()).await);
    assert!(validate("[::1]", policy.clone()).await);
    assert!(!validate("10.0.0.1", policy).await);
}

#[tokio::test]
async fn rejects_a_url_if_any_resolved_address_is_denied() {
    let result = DestinationPolicy::new(NetworkPolicy::default()).validate_resolved(
        "https://example.test/".parse().unwrap(),
        vec![
            "93.184.216.34:443".parse().unwrap(),
            "127.0.0.1:443".parse().unwrap(),
        ],
    );

    assert!(result.is_err());
}

#[test]
fn resolved_validation_cannot_bypass_url_policy() {
    let destination = DestinationPolicy::new(NetworkPolicy::default());
    let addresses = vec!["93.184.216.34:443".parse().unwrap()];

    assert!(destination
        .validate_resolved("file:///etc/passwd".parse().unwrap(), addresses.clone())
        .is_err());
    assert!(destination
        .validate_resolved(
            "https://user:pass@example.test/".parse().unwrap(),
            addresses,
        )
        .is_err());
}
