//! Managed Lantunnel 2.0 Gateway resolution.
//!
//! The Platform learns only the public Peer membership and a short-lived
//! proof of possession. The imported Peer private key never leaves tp-core's
//! signing boundary.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use reqwest::header::{ACCEPT, CACHE_CONTROL, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use tp_core::provisioning::{
    normalize_certificate_pem, GatewayBootstrapV2, PeerBootstrapV2, PeerProfileV2,
    PublicPeerMembershipV2,
};
use uuid::Uuid;
use x509_parser::extensions::GeneralName;
use x509_parser::parse_x509_certificate;

const MAX_RESOLVE_RESPONSE_BYTES: usize = 64 * 1024;

#[derive(Serialize)]
struct ManagedResolveRequestV2 {
    public_peer_membership: PublicPeerMembershipV2,
    timestamp: u64,
    request_id: String,
    proof: String,
}

// Deliberately tolerant of unknown fields. A strict reader turns every future
// Platform addition into a hard failure for already-released Clients, which is
// exactly what happened when the mapping port became a Gateway fact.
#[derive(Deserialize)]
struct ManagedResolveResponseV2 {
    gateway: ManagedGatewayFactsV2,
}

#[derive(Deserialize)]
struct ManagedGatewayFactsV2 {
    transport: String,
    dial_address: String,
    port: u16,
    /// Absent when the Gateway host still reflects on the shared default.
    #[serde(default)]
    mapping_port: Option<u16>,
    tls_server_name: String,
    trusted_certificate_pem: String,
}

/// Resolve the current Gateway facts for one imported Managed `.peer`.
///
/// This does not cache or rewrite the `.peer`. `Engine` invokes it for each
/// full Gateway Attachment generation and keeps the returned facts in memory
/// for that generation only.
pub async fn resolve_managed_gateway(
    profile: &PeerProfileV2,
) -> anyhow::Result<GatewayBootstrapV2> {
    profile.verify().context("invalid Managed Peer profile")?;
    let PeerBootstrapV2::ManagedPlatform { .. } = &profile.bootstrap else {
        bail!("Static Peer profiles do not require Platform Gateway resolution");
    };
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system time is before Unix epoch")?
        .as_secs();
    let request_id = Uuid::new_v4().to_string();
    let request = build_request(profile, timestamp, request_id)?;
    let endpoint = managed_resolve_endpoint(profile)?;
    let response = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("could not initialize Managed resolve HTTPS client")?
        .post(endpoint)
        .header(CONTENT_TYPE, "application/json")
        .header(ACCEPT, "application/json")
        .header(CACHE_CONTROL, "no-store")
        .json(&request)
        .send()
        .await
        .context("Managed Gateway resolve request failed")?;
    let status = response.status();
    if !status.is_success() {
        bail!("Managed Gateway resolve returned HTTP {status}");
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESOLVE_RESPONSE_BYTES as u64)
    {
        bail!("Managed Gateway resolve response is too large");
    }
    let bytes =
        read_response_bytes_bounded(response.bytes_stream(), MAX_RESOLVE_RESPONSE_BYTES).await?;
    parse_resolve_response(&bytes)
}

fn managed_resolve_endpoint(profile: &PeerProfileV2) -> anyhow::Result<String> {
    let PeerBootstrapV2::ManagedPlatform { platform_url } = &profile.bootstrap else {
        bail!("Static Peer profiles do not require Platform Gateway resolution");
    };
    Ok(format!(
        "{}/api/tunnels/{}/resolve",
        platform_url.trim_end_matches('/'),
        profile.tunnel_id,
    ))
}

fn parse_resolve_response(bytes: &[u8]) -> anyhow::Result<GatewayBootstrapV2> {
    let resolved: ManagedResolveResponseV2 =
        serde_json::from_slice(bytes).context("invalid Managed Gateway resolve response")?;
    let public_ip = validate_managed_origin_address(&resolved.gateway.dial_address)?;
    if resolved.gateway.tls_server_name != resolved.gateway.dial_address {
        bail!("Platform returned a Managed Gateway TLS server name that does not match its IP");
    }
    let mapping_port = resolved
        .gateway
        .mapping_port
        .unwrap_or(tp_core::config::DEFAULT_GATEWAY_MAPPING_PROBE_PORT);
    if mapping_port == 0 {
        bail!("Platform returned a Managed Gateway mapping port of zero");
    }
    // A UDP data plane and the UDP mapping socket cannot share a port. Which
    // port that is now comes from the Gateway rather than from a constant.
    if resolved.gateway.transport == "quic" && resolved.gateway.port == mapping_port {
        bail!("Managed QUIC Gateway data port conflicts with its UDP mapping port");
    }
    validate_managed_exact_leaf_pem(&resolved.gateway.trusted_certificate_pem, public_ip)?;
    let gateway = GatewayBootstrapV2 {
        transport: resolved.gateway.transport,
        dial_address: resolved.gateway.dial_address,
        port: resolved.gateway.port,
        mapping_port: Some(mapping_port),
        tls_server_name: Some(resolved.gateway.tls_server_name),
        trusted_certificate_pem: Some(resolved.gateway.trusted_certificate_pem),
    };
    gateway
        .validate()
        .context("Platform returned invalid Gateway facts")?;
    Ok(gateway)
}

fn validate_managed_exact_leaf_pem(value: &str, public_ip: IpAddr) -> anyhow::Result<()> {
    let normalized = normalize_certificate_pem(value)
        .context("Platform returned an invalid Managed Gateway certificate PEM")?;
    let certificates = tp_transport::tls::parse_certs(normalized.as_bytes())
        .context("Platform returned an invalid Managed Gateway certificate")?;
    if certificates.len() != 1 {
        bail!("Platform returned a Managed Gateway certificate chain instead of one exact leaf");
    }
    let leaf = certificates
        .first()
        .expect("the Managed certificate count was checked above");
    let (remainder, parsed) = parse_x509_certificate(leaf.as_ref())
        .map_err(|_| anyhow::anyhow!("Platform returned an invalid Managed Gateway leaf"))?;
    if !remainder.is_empty() {
        bail!("Platform returned a Managed Gateway leaf with trailing DER data");
    }
    if parsed
        .basic_constraints()
        .map_err(|_| {
            anyhow::anyhow!("Platform returned invalid Managed Gateway basic constraints")
        })?
        .is_some_and(|constraints| constraints.value.ca)
    {
        bail!("Managed Gateway certificate must be a non-CA server leaf");
    }
    if parsed.subject() != parsed.issuer() {
        bail!("Managed Gateway leaf must be self-signed");
    }
    parsed
        .verify_signature(None)
        .map_err(|_| anyhow::anyhow!("Managed Gateway leaf self-signature is invalid"))?;
    if !parsed.validity().is_valid() {
        bail!("Managed Gateway leaf must be currently valid");
    }
    if parsed
        .key_usage()
        .map_err(|_| anyhow::anyhow!("Platform returned invalid Managed Gateway key usage"))?
        .is_some_and(|usage| {
            !usage.value.digital_signature()
                || usage.value.key_cert_sign()
                || usage.value.crl_sign()
        })
    {
        bail!("Managed Gateway leaf key usage is not valid for TLS server authentication");
    }
    if parsed
        .extended_key_usage()
        .map_err(|_| {
            anyhow::anyhow!("Platform returned invalid Managed Gateway extended key usage")
        })?
        .is_some_and(|usage| !usage.value.server_auth && !usage.value.any)
    {
        bail!("Managed Gateway leaf is not valid for TLS server authentication");
    }
    let subject_alt_name = parsed
        .subject_alternative_name()
        .map_err(|_| anyhow::anyhow!("Platform returned an invalid Managed Gateway leaf SAN"))?
        .ok_or_else(|| anyhow::anyhow!("Platform returned a Managed Gateway leaf without a SAN"))?;
    let expected_ip = match public_ip {
        IpAddr::V4(ip) => ip.octets().to_vec(),
        IpAddr::V6(ip) => ip.octets().to_vec(),
    };
    if subject_alt_name.value.general_names.len() != 1
        || !matches!(
            subject_alt_name.value.general_names.first(),
            Some(GeneralName::IPAddress(bytes)) if *bytes == expected_ip
        )
    {
        bail!("Managed Gateway leaf SAN must be exactly {public_ip}");
    }
    Ok(())
}

fn validate_managed_origin_address(value: &str) -> anyhow::Result<IpAddr> {
    let address: IpAddr = value
        .parse()
        .context("Platform returned a Managed Gateway origin that is not an IP address")?;
    if address.to_string() != value || !is_public_managed_origin(address) {
        bail!("Platform returned a Managed Gateway origin that is not a canonical public IP");
    }
    Ok(address)
}

fn is_public_managed_origin(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_managed_origin_v4(address),
        IpAddr::V6(address) => is_public_managed_origin_v6(address),
    }
}

fn is_public_managed_origin_v4(address: Ipv4Addr) -> bool {
    let [first, second, _, _] = address.octets();
    !(first == 0
        || address.is_private()
        || address.is_loopback()
        || address.is_link_local()
        || address.is_broadcast()
        || address.is_documentation()
        || address.is_multicast()
        || (first == 100 && (64..=127).contains(&second))
        || (first == 198 && (18..=19).contains(&second))
        || first >= 240)
}

fn is_public_managed_origin_v6(address: Ipv6Addr) -> bool {
    if let Some(mapped) = address.to_ipv4_mapped() {
        return is_public_managed_origin_v4(mapped);
    }
    let segments = address.segments();
    !(address.is_unspecified()
        || address.is_loopback()
        || address.is_multicast()
        || address.is_unicast_link_local()
        || segments[0] & 0xfe00 == 0xfc00
        || (segments[0] == 0x2001 && segments[1] == 0x0db8))
}

async fn read_response_bytes_bounded<S, E>(
    mut chunks: S,
    max_bytes: usize,
) -> anyhow::Result<Vec<u8>>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: std::error::Error + Send + Sync + 'static,
{
    let mut bytes = Vec::with_capacity(max_bytes.saturating_add(1));
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.context("could not read Managed Gateway resolve response")?;
        let remaining = max_bytes.saturating_add(1).saturating_sub(bytes.len());
        bytes.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        if bytes.len() > max_bytes {
            bail!("Managed Gateway resolve response is too large");
        }
    }
    Ok(bytes)
}

fn build_request(
    profile: &PeerProfileV2,
    timestamp: u64,
    request_id: String,
) -> anyhow::Result<ManagedResolveRequestV2> {
    let proof = profile
        .sign_managed_resolve_proof(timestamp, &request_id)
        .context("could not sign Managed Gateway resolve proof")?;
    Ok(ManagedResolveRequestV2 {
        public_peer_membership: profile.public_membership(),
        timestamp,
        request_id,
        proof,
    })
}

// Test fixtures in this file use 1.1.1.1 rather than an RFC 5737
// documentation address on purpose: the code under test validates that a
// Gateway address is globally routable, and `Ipv4Addr::is_documentation`
// rejects 192.0.2.0/24, 198.51.100.0/24, and 203.0.113.0/24. Elsewhere in the
// workspace the documentation ranges are the right choice.
#[cfg(test)]
mod tests {
    use serde_json::json;
    use tp_core::provisioning::{GatewayBootstrapV2, PeerBootstrapV2, TunnelOwnerFileV2};

    fn managed_profile() -> tp_core::provisioning::PeerProfileV2 {
        let mut owner = TunnelOwnerFileV2::generate(GatewayBootstrapV2 {
            transport: "quic".into(),
            dial_address: "gateway.example".into(),
            port: 8443,
            mapping_port: None,
            tls_server_name: Some("gateway.example".into()),
            trusted_certificate_pem: None,
        })
        .expect("Tunnel");
        let mut profile = owner.add_peer(None, 1, None).expect("Peer");
        profile.bootstrap = PeerBootstrapV2::ManagedPlatform {
            platform_url: "https://platform.example".into(),
        };
        profile
    }

    #[test]
    fn request_contains_only_public_membership_and_a_verifiable_proof() {
        let profile = managed_profile();
        let request_id = "018f6e84-e11b-7f3a-8cad-9f68f4482001".to_string();
        let request =
            super::build_request(&profile, 1_786_426_560, request_id.clone()).expect("request");

        assert_eq!(request.public_peer_membership, profile.public_membership());
        request
            .public_peer_membership
            .verify_managed_resolve_proof(request.timestamp, &request.request_id, &request.proof)
            .expect("proof");
        let json = serde_json::to_string(&request).expect("json");
        assert!(!json.contains(profile.peer.peer_private_key.as_str()));
        assert!(!json.contains("gateway.example"));
    }

    #[test]
    fn managed_resolve_uses_the_canonical_unversioned_user_api() {
        let profile = managed_profile();

        assert_eq!(
            super::managed_resolve_endpoint(&profile).expect("resolve endpoint"),
            format!(
                "https://platform.example/api/tunnels/{}/resolve",
                profile.tunnel_id,
            ),
        );
    }

    #[tokio::test]
    async fn streamed_response_rejects_the_first_byte_past_the_limit() {
        let chunks = futures_util::stream::iter([
            Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"1234")),
            Ok(bytes::Bytes::from_static(b"5ignored")),
        ]);

        let error = super::read_response_bytes_bounded(chunks, 4)
            .await
            .expect_err("fifth byte must exceed the response limit")
            .to_string();

        assert!(error.contains("too large"));
    }

    #[tokio::test]
    async fn streamed_response_accepts_exactly_the_limit() {
        let chunks = futures_util::stream::iter([
            Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"12")),
            Ok(bytes::Bytes::from_static(b"34")),
        ]);

        assert_eq!(
            super::read_response_bytes_bounded(chunks, 4)
                .await
                .expect("bounded response"),
            b"1234"
        );
    }

    #[test]
    fn managed_gateway_accepts_direct_ip_with_matching_tls_identity_and_exact_leaf_pem() {
        let certified =
            rcgen::generate_simple_self_signed(vec!["1.1.1.1".into()]).expect("test certificate");
        let certificate_pem = certified.cert.pem();
        let response = serde_json::to_vec(&json!({
            "gateway": {
                "transport": "quic",
                "dial_address": "1.1.1.1",
                "port": 8443,
                "tls_server_name": "1.1.1.1",
                "trusted_certificate_pem": certificate_pem
            }
        }))
        .expect("response");

        let gateway = super::parse_resolve_response(&response).expect("managed Gateway facts");

        assert_eq!(gateway.transport, "quic");
        assert_eq!(gateway.dial_address, "1.1.1.1");
        assert_eq!(gateway.port, 8443);
        assert_eq!(gateway.tls_server_name.as_deref(), Some("1.1.1.1"));
        assert_eq!(
            gateway.trusted_certificate_pem.as_deref(),
            Some(certificate_pem.as_str())
        );
    }

    #[test]
    fn managed_gateway_rejects_a_leaf_for_a_different_ip() {
        let certified = rcgen::generate_simple_self_signed(vec!["192.0.2.89".into()])
            .expect("test certificate");
        let response = serde_json::to_vec(&json!({
            "gateway": {
                "transport": "quic",
                "dial_address": "1.1.1.1",
                "port": 8443,
                "tls_server_name": "1.1.1.1",
                "trusted_certificate_pem": certified.cert.pem()
            }
        }))
        .expect("response");

        let error = match super::parse_resolve_response(&response) {
            Ok(_) => panic!("Managed leaf must name the exact public IP"),
            Err(error) => error.to_string(),
        };

        assert!(error.contains("SAN must be exactly 1.1.1.1"));
    }

    #[test]
    fn managed_gateway_rejects_a_ca_certificate() {
        let mut params =
            rcgen::CertificateParams::new(vec!["1.1.1.1".into()]).expect("certificate params");
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let key = rcgen::KeyPair::generate().expect("test key");
        let certificate_pem = params.self_signed(&key).expect("CA certificate").pem();
        let response = serde_json::to_vec(&json!({
            "gateway": {
                "transport": "quic",
                "dial_address": "1.1.1.1",
                "port": 8443,
                "tls_server_name": "1.1.1.1",
                "trusted_certificate_pem": certificate_pem
            }
        }))
        .expect("response");

        let error = match super::parse_resolve_response(&response) {
            Ok(_) => panic!("Managed identity must be a non-CA server leaf"),
            Err(error) => error.to_string(),
        };

        assert!(error.contains("non-CA server leaf"));
    }

    #[test]
    fn managed_gateway_rejects_a_leaf_not_signed_by_its_own_key() {
        let mut issuer_params =
            rcgen::CertificateParams::new(Vec::<String>::new()).expect("issuer params");
        issuer_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        issuer_params.distinguished_name = rcgen::DistinguishedName::new();
        issuer_params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "same subject");
        let issuer_key = rcgen::KeyPair::generate().expect("issuer key");
        let issuer = issuer_params
            .self_signed(&issuer_key)
            .expect("issuer certificate");
        let mut leaf_params =
            rcgen::CertificateParams::new(vec!["1.1.1.1".into()]).expect("leaf params");
        leaf_params.distinguished_name = rcgen::DistinguishedName::new();
        leaf_params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "same subject");
        let leaf_key = rcgen::KeyPair::generate().expect("leaf key");
        let certificate_pem = leaf_params
            .signed_by(&leaf_key, &issuer, &issuer_key)
            .expect("issuer-signed leaf")
            .pem();
        let response = serde_json::to_vec(&json!({
            "gateway": {
                "transport": "quic",
                "dial_address": "1.1.1.1",
                "port": 8443,
                "tls_server_name": "1.1.1.1",
                "trusted_certificate_pem": certificate_pem
            }
        }))
        .expect("response");

        let error = match super::parse_resolve_response(&response) {
            Ok(_) => panic!("Managed leaf must verify with its own public key"),
            Err(error) => error.to_string(),
        };

        assert!(error.contains("self-signature"));
    }

    #[test]
    fn managed_gateway_rejects_an_expired_leaf() {
        let mut params =
            rcgen::CertificateParams::new(vec!["1.1.1.1".into()]).expect("certificate params");
        params.not_before = rcgen::date_time_ymd(1999, 1, 1);
        params.not_after = rcgen::date_time_ymd(2000, 1, 1);
        let key = rcgen::KeyPair::generate().expect("test key");
        let certificate_pem = params.self_signed(&key).expect("expired leaf").pem();
        let response = serde_json::to_vec(&json!({
            "gateway": {
                "transport": "quic",
                "dial_address": "1.1.1.1",
                "port": 8443,
                "tls_server_name": "1.1.1.1",
                "trusted_certificate_pem": certificate_pem
            }
        }))
        .expect("response");

        let error = match super::parse_resolve_response(&response) {
            Ok(_) => panic!("Managed leaf must be currently valid"),
            Err(error) => error.to_string(),
        };

        assert!(error.contains("currently valid"));
    }

    #[test]
    fn managed_gateway_rejects_a_leaf_not_valid_for_tls_server_auth() {
        let mut params =
            rcgen::CertificateParams::new(vec!["1.1.1.1".into()]).expect("certificate params");
        params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ClientAuth];
        let key = rcgen::KeyPair::generate().expect("test key");
        let certificate_pem = params.self_signed(&key).expect("client-only leaf").pem();
        let response = serde_json::to_vec(&json!({
            "gateway": {
                "transport": "quic",
                "dial_address": "1.1.1.1",
                "port": 8443,
                "tls_server_name": "1.1.1.1",
                "trusted_certificate_pem": certificate_pem
            }
        }))
        .expect("response");

        let error = match super::parse_resolve_response(&response) {
            Ok(_) => panic!("Managed leaf must support TLS server auth"),
            Err(error) => error.to_string(),
        };

        assert!(error.contains("TLS server authentication"));
    }

    #[test]
    fn managed_gateway_rejects_legacy_or_non_exact_tls_trust() {
        let certified =
            rcgen::generate_simple_self_signed(vec!["1.1.1.1".into()]).expect("test certificate");
        let certificate_pem = certified.cert.pem();
        let extra = rcgen::generate_simple_self_signed(vec!["1.1.1.1".into()])
            .expect("extra test certificate");

        let assigned_hostname = serde_json::to_vec(&json!({
            "gateway": {
                "transport": "quic",
                "dial_address": "1.1.1.1",
                "port": 8443,
                "tls_server_name": "gw-018f6e84-e11b-7f3a-8cad-9f68f4482001.lantunnel.app",
                "trusted_certificate_pem": certificate_pem
            }
        }))
        .expect("response");
        let origin_ca_anchor = serde_json::to_vec(&json!({
            "gateway": {
                "transport": "quic",
                "dial_address": "1.1.1.1",
                "port": 8443,
                "tls_server_name": "1.1.1.1",
                "trust_anchor": "cloudflare_origin_ca_ecc",
                "trusted_certificate_pem": certificate_pem
            }
        }))
        .expect("response");
        let certificate_chain = serde_json::to_vec(&json!({
            "gateway": {
                "transport": "quic",
                "dial_address": "1.1.1.1",
                "port": 8443,
                "tls_server_name": "1.1.1.1",
                "trusted_certificate_pem": format!("{certificate_pem}{}", extra.cert.pem())
            }
        }))
        .expect("response");

        for response in [assigned_hostname, certificate_chain] {
            assert!(super::parse_resolve_response(&response).is_err());
        }

        // A legacy trust hint is ignored rather than rejected: the reader is
        // forward compatible, and trust comes from the exact leaf pinned to the
        // dialled IP, never from an anchor name the Platform sends.
        let ignored_anchor =
            super::parse_resolve_response(&origin_ca_anchor).expect("anchor hint is not trust");
        assert_eq!(
            ignored_anchor.trusted_certificate_pem.as_deref(),
            Some(certificate_pem.as_str())
        );
    }

    #[test]
    fn managed_gateway_rejects_non_public_or_mismatched_ip_identities() {
        let certified =
            rcgen::generate_simple_self_signed(vec!["1.1.1.1".into()]).expect("test certificate");
        let certificate_pem = certified.cert.pem();
        for (dial_address, tls_server_name) in [
            ("127.0.0.1", "127.0.0.1"),
            ("10.0.0.7", "10.0.0.7"),
            ("1.1.1.1", "192.0.2.89"),
            ("01.1.1.1", "01.1.1.1"),
        ] {
            let response = serde_json::to_vec(&json!({
                "gateway": {
                    "transport": "quic",
                    "dial_address": dial_address,
                    "port": 8443,
                    "tls_server_name": tls_server_name,
                    "trusted_certificate_pem": certificate_pem
                }
            }))
            .expect("response");

            assert!(
                super::parse_resolve_response(&response).is_err(),
                "must reject origin={dial_address} SNI={tls_server_name}"
            );
        }
    }

    #[test]
    fn managed_quic_rejects_the_shared_udp_mapping_port_but_tcp_carriers_allow_it() {
        let certified =
            rcgen::generate_simple_self_signed(vec!["1.1.1.1".into()]).expect("test certificate");
        let certificate_pem = certified.cert.pem();
        let response = |transport: &str| {
            serde_json::to_vec(&json!({
                "gateway": {
                    "transport": transport,
                    "dial_address": "1.1.1.1",
                    "port": 8444,
                    "tls_server_name": "1.1.1.1",
                    "trusted_certificate_pem": certificate_pem
                }
            }))
            .expect("response")
        };

        assert!(super::parse_resolve_response(&response("quic")).is_err());
        assert!(super::parse_resolve_response(&response("websocket")).is_ok());
        assert!(super::parse_resolve_response(&response("grpc")).is_ok());
    }

    #[test]
    fn the_mapping_port_comes_from_the_gateway_and_defaults_only_when_absent() {
        let certified =
            rcgen::generate_simple_self_signed(vec!["1.1.1.1".into()]).expect("test certificate");
        let certificate_pem = certified.cert.pem();
        let response = |gateway: serde_json::Value| {
            serde_json::to_vec(&json!({ "gateway": gateway })).expect("response")
        };
        let facts = |mapping: Option<u16>, data_port: u16| {
            let mut gateway = json!({
                "transport": "quic",
                "dial_address": "1.1.1.1",
                "port": data_port,
                "tls_server_name": "1.1.1.1",
                "trusted_certificate_pem": certificate_pem
            });
            if let Some(mapping) = mapping {
                gateway["mapping_port"] = json!(mapping);
            }
            gateway
        };

        // A Gateway that never moved sends nothing, and the Client keeps
        // probing the shared default.
        let default_port = super::parse_resolve_response(&response(facts(None, 8443)))
            .expect("resolve without a mapping port");
        assert_eq!(default_port.mapping_port, Some(8444));

        let moved = super::parse_resolve_response(&response(facts(Some(10_444), 8443)))
            .expect("resolve with a mapping port");
        assert_eq!(moved.mapping_port, Some(10_444));

        // The collision rule follows the reported port rather than 8444.
        assert!(super::parse_resolve_response(&response(facts(Some(10_444), 10_444))).is_err());
        assert!(super::parse_resolve_response(&response(facts(Some(10_444), 8444))).is_ok());
    }

    #[test]
    fn an_unknown_platform_field_does_not_break_an_older_client() {
        let certified =
            rcgen::generate_simple_self_signed(vec!["1.1.1.1".into()]).expect("test certificate");
        let response = serde_json::to_vec(&json!({
            "gateway": {
                "transport": "quic",
                "dial_address": "1.1.1.1",
                "port": 8443,
                "tls_server_name": "1.1.1.1",
                "trusted_certificate_pem": certified.cert.pem(),
                "something_the_platform_added_later": true
            }
        }))
        .expect("response");

        super::parse_resolve_response(&response).expect("unknown fields must be ignored");
    }
}
