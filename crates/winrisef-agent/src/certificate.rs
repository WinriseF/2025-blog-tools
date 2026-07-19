use std::net::IpAddr;

use anyhow::Context;
use rcgen::{CertificateParams, KeyPair, PKCS_ECDSA_P256_SHA256};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};
use time::{Duration, OffsetDateTime};

pub struct EphemeralIdentity {
    pub certificate_chain: Vec<CertificateDer<'static>>,
    pub private_key: PrivateKeyDer<'static>,
    pub sha256: [u8; 32],
}

pub fn install_crypto_provider() -> anyhow::Result<()> {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        rustls::crypto::ring::default_provider()
            .install_default()
            .map_err(|_| anyhow::anyhow!("failed to install the rustls ring crypto provider"))?;
        tracing::debug!(
            provider = "ring",
            "installed default rustls crypto provider"
        );
    } else {
        tracing::debug!("rustls crypto provider was already installed");
    }
    Ok(())
}

pub fn generate_identity(additional_ips: &[IpAddr]) -> anyhow::Result<EphemeralIdentity> {
    let mut names = vec![
        "localhost".to_owned(),
        "127.0.0.1".to_owned(),
        "::1".to_owned(),
    ];
    for ip in additional_ips {
        let name = ip.to_string();
        if !names.contains(&name) {
            names.push(name);
        }
    }
    let subject_alt_name_count = names.len();
    let mut params =
        CertificateParams::new(names).context("failed to create certificate parameters")?;
    let now = OffsetDateTime::now_utc();
    params.not_before = now - Duration::minutes(5);
    params.not_after = now + Duration::days(13);
    tracing::debug!(
        subject_alt_name_count,
        additional_ip_count = additional_ips.len(),
        not_before = %params.not_before,
        not_after = %params.not_after,
        algorithm = "ECDSA-P256-SHA256",
        "generating ephemeral WebTransport certificate"
    );

    let key_pair = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)
        .context("failed to create P-256 certificate key")?;
    let certificate = params
        .self_signed(&key_pair)
        .context("failed to self-sign the ephemeral certificate")?;
    let certificate_der = certificate.der().clone();
    let sha256: [u8; 32] = Sha256::digest(certificate_der.as_ref()).into();
    let private_key = PrivatePkcs8KeyDer::from(key_pair.serialize_der()).into();

    tracing::info!(
        certificate_sha256 = %format_sha256(&sha256),
        certificate_der_bytes = certificate_der.as_ref().len(),
        "generated ephemeral WebTransport certificate"
    );
    Ok(EphemeralIdentity {
        certificate_chain: vec![certificate_der],
        private_key,
        sha256,
    })
}

pub fn format_sha256(bytes: &[u8; 32]) -> String {
    format_hex(bytes)
}

pub fn format_hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}
