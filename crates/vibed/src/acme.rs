//! The external address's certificate (PRD: External access): from Let's
//! Encrypt over ACME, proven with TLS-ALPN-01 on port 443 itself, so
//! nothing but 443 need be open. It's kept on disk, private to the owner,
//! and renewed once a third of its life is left.

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use instant_acme::{
    Account, AccountCredentials, AuthorizationStatus, ChallengeType, Identifier, NewAccount,
    NewOrder, OrderStatus, RetryPolicy,
};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use vibe_mcp::public::ACME_TLS_ALPN;

/// Let's Encrypt's production directory.
pub const LETS_ENCRYPT: &str = "https://acme-v02.api.letsencrypt.org/directory";

/// How long Let's Encrypt gets to check a challenge, and then to issue.
const ORDER_PATIENCE: Duration = Duration::from_secs(120);

/// When a certificate is good from and until, in Unix seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Validity {
    pub not_before: i64,
    pub not_after: i64,
}

impl Validity {
    /// When it's renewed: once two thirds of its life is gone, which is day
    /// 60 of Let's Encrypt's 90, and keeps up as their lives get shorter.
    pub fn renew_at(&self) -> i64 {
        self.not_before + (self.not_after - self.not_before) * 2 / 3
    }
}

/// A certificate chain and its key, ready to serve.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub key: Arc<CertifiedKey>,
    pub validity: Validity,
}

/// Reads a PEM chain and its PEM key, and checks the key is the chain's.
pub fn load_pem(chain_pem: &[u8], key_pem: &[u8]) -> Result<Loaded, String> {
    let chain = CertificateDer::pem_slice_iter(chain_pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("its certificate can't be read: {e}"))?;
    let first = chain.first().ok_or("it holds no certificate")?;
    let validity = validity(first).ok_or("its certificate's dates can't be read")?;
    let key = PrivateKeyDer::from_pem_slice(key_pem)
        .map_err(|e| format!("its key can't be read: {e}"))?;
    let provider = rustls::crypto::ring::default_provider();
    let key = CertifiedKey::from_der(chain, key, &provider)
        .map_err(|e| format!("its key isn't its certificate's: {e}"))?;
    Ok(Loaded {
        key: Arc::new(key),
        validity,
    })
}

// A little DER, for a certificate's validity.

/// One DER element: its tag, its contents, and what follows it.
fn tlv(der: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = der.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (len, rest) = if first < 0x80 {
        (usize::from(first), rest)
    } else {
        let n = usize::from(first & 0x7f);
        if n == 0 || n > 4 || rest.len() < n {
            return None;
        }
        let len = rest[..n]
            .iter()
            .fold(0usize, |acc, b| (acc << 8) | usize::from(*b));
        (len, &rest[n..])
    };
    (rest.len() >= len).then(|| (tag, &rest[..len], &rest[len..]))
}

/// A UTCTime (`YYMMDDHHMMSSZ`) or GeneralizedTime (`YYYYMMDDHHMMSSZ`), in
/// Unix seconds.
fn der_time(tag: u8, value: &[u8]) -> Option<i64> {
    let s = std::str::from_utf8(value).ok()?.strip_suffix('Z')?;
    if !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (year, rest) = match (tag, s.len()) {
        (0x17, 12) => {
            let yy: i64 = s[..2].parse().ok()?;
            (if yy < 50 { 2000 + yy } else { 1900 + yy }, &s[2..])
        }
        (0x18, 14) => (s[..4].parse().ok()?, &s[4..]),
        _ => return None,
    };
    let n = |i: usize| rest[i..i + 2].parse::<u32>().ok();
    vibe_core::time::unix_seconds(year, n(0)?, n(2)?, n(4)?, n(6)?, n(8)?)
}

/// When a DER certificate is valid, from its `tbsCertificate`.
pub fn validity(cert: &[u8]) -> Option<Validity> {
    let (0x30, cert, _) = tlv(cert)? else {
        return None;
    };
    let (0x30, tbs, _) = tlv(cert)? else {
        return None;
    };
    let mut rest = tbs;
    // [0] version, if there is one; then the serial, the signature
    // algorithm and the issuer.
    let (tag, _, after) = tlv(rest)?;
    if tag == 0xa0 {
        rest = after;
    }
    for want in [0x02, 0x30, 0x30] {
        let (tag, _, after) = tlv(rest)?;
        if tag != want {
            return None;
        }
        rest = after;
    }
    let (0x30, times, _) = tlv(rest)? else {
        return None;
    };
    let (t1, v1, times) = tlv(times)?;
    let (t2, v2, _) = tlv(times)?;
    Some(Validity {
        not_before: der_time(t1, v1)?,
        not_after: der_time(t2, v2)?,
    })
}

// What the TLS listener serves.

/// The external address's certificates: the one it serves, and while Let's
/// Encrypt checks it, the TLS-ALPN-01 challenge's.
#[derive(Debug)]
pub struct Certs {
    domain: String,
    current: RwLock<Option<Loaded>>,
    challenge: RwLock<Option<Arc<CertifiedKey>>>,
}

impl Certs {
    pub fn new(domain: &str) -> Certs {
        Certs {
            domain: domain.to_owned(),
            current: RwLock::new(None),
            challenge: RwLock::new(None),
        }
    }

    pub fn domain(&self) -> &str {
        &self.domain
    }

    pub fn set(&self, loaded: Loaded) {
        *self.current.write().unwrap_or_else(|e| e.into_inner()) = Some(loaded);
    }

    /// The certificate served now, if there is one.
    pub fn current(&self) -> Option<Loaded> {
        self.current
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn set_challenge(&self, cert: Option<Arc<CertifiedKey>>) {
        *self.challenge.write().unwrap_or_else(|e| e.into_inner()) = cert;
    }

    /// What to serve a client that asks for `name` (its SNI), and whether
    /// it asked for `acme-tls/1`. A name other than the domain gets nothing,
    /// so a scanner learns nothing; a client that names none (by IP) gets
    /// the certificate, which then doesn't match.
    fn pick(&self, name: Option<&str>, acme: bool) -> Option<Arc<CertifiedKey>> {
        let ours = name.is_some_and(|n| n.eq_ignore_ascii_case(&self.domain));
        if acme {
            // Only Let's Encrypt asks this, naming the domain it checks.
            return if ours {
                self.challenge
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
            } else {
                None
            };
        }
        if name.is_some() && !ours {
            return None;
        }
        self.current().map(|l| l.key)
    }
}

impl ResolvesServerCert for Certs {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let acme = hello
            .alpn()
            .is_some_and(|mut protocols| protocols.any(|p| p == ACME_TLS_ALPN));
        self.pick(hello.server_name(), acme)
    }
}

/// The certificate that answers a TLS-ALPN-01 challenge (RFC 8737): self
/// signed, for the domain, carrying the key authorization's digest in a
/// critical `acmeIdentifier` extension.
pub fn challenge_cert(domain: &str, digest: &[u8]) -> Result<Arc<CertifiedKey>, String> {
    let fail = |e: rcgen::Error| format!("can't make the challenge's certificate: {e}");
    let mut params = rcgen::CertificateParams::new(vec![domain.to_owned()]).map_err(fail)?;
    params.custom_extensions = vec![rcgen::CustomExtension::new_acme_identifier(digest)];
    let key = rcgen::KeyPair::generate().map_err(fail)?;
    let cert = params.self_signed(&key).map_err(fail)?;
    let der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
    let signing = rustls::crypto::ring::sign::any_supported_type(&der)
        .map_err(|e| format!("can't use the challenge's key: {e}"))?;
    Ok(Arc::new(CertifiedKey::new(
        vec![cert.der().clone()],
        signing,
    )))
}

// On disk.

/// Where the certificates and the ACME account live: `external/` in the
/// state directory, private to the owner.
#[derive(Debug, Clone)]
pub struct Disk {
    dir: PathBuf,
}

/// Writes `bytes` to `path` whole, readable by the owner only.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("new");
    let mut f = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    fs::rename(&tmp, path)
}

impl Disk {
    pub fn new(dir: PathBuf) -> Disk {
        Disk { dir }
    }

    fn ensure(&self) -> Result<(), String> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)
            .and_then(|()| vibe_core::paths::private_dir(&self.dir))
            .map_err(|e| format!("can't make {}: {e}", self.dir.display()))
    }

    /// The account for `directory`: one per ACME server, as an account is
    /// that server's.
    fn account_path(&self, directory: &str) -> PathBuf {
        let tag = &crate::remote::sha256_hex(directory.as_bytes())[..12];
        self.dir.join(format!("account-{tag}.json"))
    }

    fn chain_path(&self, domain: &str) -> PathBuf {
        self.dir.join(format!("{domain}.crt"))
    }

    fn key_path(&self, domain: &str) -> PathBuf {
        self.dir.join(format!("{domain}.key"))
    }

    /// The certificate kept for `domain`, if there's one and it reads.
    pub fn load(&self, domain: &str) -> Option<Result<Loaded, String>> {
        let chain = fs::read(self.chain_path(domain)).ok()?;
        let key = fs::read(self.key_path(domain)).ok()?;
        Some(load_pem(&chain, &key))
    }

    pub fn save(&self, domain: &str, chain_pem: &str, key_pem: &str) -> Result<(), String> {
        self.ensure()?;
        let fail = |e: std::io::Error| format!("can't keep the certificate: {e}");
        // The key first: a chain without its key is never read alone.
        write_private(&self.key_path(domain), key_pem.as_bytes()).map_err(fail)?;
        write_private(&self.chain_path(domain), chain_pem.as_bytes()).map_err(fail)
    }

    async fn account(&self, directory: &str) -> Result<Account, String> {
        let fail = |e: instant_acme::Error| format!("Let's Encrypt: {e}");
        let builder = || Account::builder().map_err(fail);
        let path = self.account_path(directory);
        // An account file that's there but won't read is an error to fix,
        // not a reason for a new account each try (they're rate-limited).
        match fs::read(&path) {
            Ok(json) => {
                let creds = serde_json::from_slice::<AccountCredentials>(&json).map_err(|e| {
                    format!("the ACME account in {} can't be read: {e}", path.display())
                })?;
                return builder()?.from_credentials(creds).await.map_err(fail);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(format!(
                    "the ACME account in {} can't be read: {e}",
                    path.display()
                ));
            }
        }
        let (account, creds) = builder()?
            .create(
                &NewAccount {
                    contact: &[],
                    terms_of_service_agreed: true,
                    only_return_existing: false,
                },
                directory.to_owned(),
                None,
            )
            .await
            .map_err(fail)?;
        self.ensure()?;
        let json =
            serde_json::to_vec(&creds).map_err(|e| format!("can't keep the account: {e}"))?;
        write_private(&path, &json).map_err(|e| format!("can't keep the account: {e}"))?;
        Ok(account)
    }
}

/// Gets a certificate for `certs`'s domain from the ACME server at
/// `directory`, answering its TLS-ALPN-01 challenge through `certs` (which
/// the listener on port 443 serves), keeps it on `disk`, and serves it.
pub async fn obtain(directory: &str, disk: &Disk, certs: &Certs) -> Result<Validity, String> {
    let domain = certs.domain().to_owned();
    let fail = |e: instant_acme::Error| format!("Let's Encrypt: {e}");
    let account = disk.account(directory).await?;
    // However the order ends, no challenge is left to serve.
    struct Clear<'a>(&'a Certs);
    impl Drop for Clear<'_> {
        fn drop(&mut self) {
            self.0.set_challenge(None);
        }
    }
    let _clear = Clear(certs);
    let ids = [Identifier::Dns(domain.clone())];
    let mut order = account
        .new_order(&NewOrder::new(&ids))
        .await
        .map_err(fail)?;
    let mut authorizations = order.authorizations();
    while let Some(authz) = authorizations.next().await {
        let mut authz = authz.map_err(fail)?;
        match authz.status {
            AuthorizationStatus::Pending => {}
            AuthorizationStatus::Valid => continue,
            other => return Err(format!("Let's Encrypt holds {domain} as {other:?}")),
        }
        let mut challenge = authz
            .challenge(ChallengeType::TlsAlpn01)
            .ok_or("Let's Encrypt offered no TLS-ALPN-01 challenge")?;
        let digest = challenge.key_authorization().digest();
        certs.set_challenge(Some(challenge_cert(&domain, digest.as_ref())?));
        challenge.set_ready().await.map_err(fail)?;
    }
    let patience = RetryPolicy::new()
        .initial_delay(Duration::from_secs(1))
        .timeout(ORDER_PATIENCE);
    let ready = order.poll_ready(&patience).await;
    certs.set_challenge(None);
    match ready.map_err(fail)? {
        OrderStatus::Ready => {}
        OrderStatus::Invalid => return Err(why_invalid(&mut order, &domain).await),
        other => return Err(format!("Let's Encrypt left the order {other:?}")),
    }
    let key_pem = order.finalize().await.map_err(fail)?;
    let chain_pem = order.poll_certificate(&patience).await.map_err(fail)?;
    let loaded = load_pem(chain_pem.as_bytes(), key_pem.as_bytes())?;
    disk.save(&domain, &chain_pem, &key_pem)?;
    let validity = loaded.validity;
    certs.set(loaded);
    Ok(validity)
}

/// Why Let's Encrypt refused: its own words about the challenge, which say
/// what it saw (no DNS record, a timeout on 443, another certificate).
async fn why_invalid(order: &mut instant_acme::Order, domain: &str) -> String {
    let mut authorizations = order.authorizations();
    while let Some(Ok(mut authz)) = authorizations.next().await {
        if let Ok(state) = authz.refresh().await
            && let Some(problem) = state.challenges.iter().find_map(|c| c.error.as_ref())
        {
            return format!(
                "Let's Encrypt couldn't check {domain}: {}. Check that {domain} points at this server and that port 443 is open to the internet",
                problem.detail.as_deref().unwrap_or("it gave no reason")
            );
        }
    }
    format!(
        "Let's Encrypt couldn't check {domain}. Check that {domain} points at this server and that port 443 is open to the internet"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn self_signed(domain: &str) -> (String, String) {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(vec![domain.to_owned()]).unwrap();
        params.not_before = rcgen::date_time_ymd(2026, 10, 1);
        params.not_after = rcgen::date_time_ymd(2026, 12, 30);
        let cert = params.self_signed(&key).unwrap();
        (cert.pem(), key.serialize_pem())
    }

    #[test]
    fn a_certificates_dates_are_read() {
        let (chain, key) = self_signed("vps.example.com");
        let l = load_pem(chain.as_bytes(), key.as_bytes()).unwrap();
        let start = vibe_core::time::unix_seconds(2026, 10, 1, 0, 0, 0).unwrap();
        let end = vibe_core::time::unix_seconds(2026, 12, 30, 0, 0, 0).unwrap();
        assert_eq!(
            l.validity,
            Validity {
                not_before: start,
                not_after: end
            }
        );
        // 90 days: renewed on day 60.
        assert_eq!(l.validity.renew_at(), start + 60 * 86_400);
        assert_eq!(
            der_time(0x18, b"20500101000000Z"),
            vibe_core::time::unix_seconds(2050, 1, 1, 0, 0, 0)
        );
        assert_eq!(der_time(0x17, b"491231235959Z").map(|t| t > 0), Some(true));
        assert_eq!(der_time(0x17, b"500101000000Z").map(|t| t < 0), Some(true));
        assert_eq!(der_time(0x17, b"2601010000Z"), None);
        assert_eq!(validity(b"\x30\x03\x02\x01\x00"), None);
        assert_eq!(validity(&[]), None);
    }

    #[test]
    fn a_key_must_be_its_certificates() {
        let (chain, _) = self_signed("vps.example.com");
        let (_, other) = self_signed("vps.example.com");
        let e = load_pem(chain.as_bytes(), other.as_bytes()).unwrap_err();
        assert!(e.contains("isn't its certificate's"), "{e}");
        assert!(load_pem(b"", other.as_bytes()).is_err());
    }

    #[test]
    fn names_pick_what_is_served() {
        let certs = Certs::new("vps.example.com");
        assert!(
            certs.pick(Some("vps.example.com"), false).is_none(),
            "none yet"
        );
        let (chain, key) = self_signed("vps.example.com");
        certs.set(load_pem(chain.as_bytes(), key.as_bytes()).unwrap());
        assert!(certs.pick(Some("VPS.example.com"), false).is_some());
        assert!(certs.pick(None, false).is_some(), "by IP");
        assert!(certs.pick(Some("other.example.com"), false).is_none());
        // The challenge only while there's one, and only for the domain.
        assert!(certs.pick(Some("vps.example.com"), true).is_none());
        let challenge = challenge_cert("vps.example.com", &[7; 32]).unwrap();
        certs.set_challenge(Some(Arc::clone(&challenge)));
        let got = certs.pick(Some("vps.example.com"), true).unwrap();
        assert!(Arc::ptr_eq(&got, &challenge));
        assert!(certs.pick(None, true).is_none());
        assert!(certs.pick(Some("other.example.com"), true).is_none());
        // Ordinary clients still get the real one meanwhile.
        let real = certs.pick(Some("vps.example.com"), false).unwrap();
        assert!(!Arc::ptr_eq(&real, &challenge));
    }

    #[test]
    fn the_challenge_carries_the_digest_in_a_critical_extension() {
        let digest = [0xab; 32];
        let c = challenge_cert("vps.example.com", &digest).unwrap();
        let der = c.cert[0].as_ref();
        // id-pe-acmeIdentifier, 1.3.6.1.5.5.7.1.31, critical, holding an
        // OCTET STRING of the 32-byte digest.
        let oid = [0x06, 0x08, 0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x01, 0x1f];
        let at = der
            .windows(oid.len())
            .position(|w| w == oid)
            .expect("the acmeIdentifier extension");
        let rest = &der[at + oid.len()..];
        assert_eq!(&rest[..3], &[0x01, 0x01, 0xff], "critical");
        assert_eq!(&rest[3..7], &[0x04, 0x22, 0x04, 0x20]);
        assert_eq!(&rest[7..39], &digest);
    }

    /// Takes whatever certificate the server shows, and keeps it.
    #[derive(Debug, Default)]
    struct Recorder(std::sync::Mutex<Option<Vec<u8>>>);

    impl rustls::client::danger::ServerCertVerifier for Recorder {
        fn verify_server_cert(
            &self,
            end: &CertificateDer<'_>,
            _: &[CertificateDer<'_>],
            _: &rustls::pki_types::ServerName<'_>,
            _: &[u8],
            _: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            *self.0.lock().unwrap() = Some(end.as_ref().to_vec());
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _: &[u8],
            _: &CertificateDer<'_>,
            _: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _: &[u8],
            _: &CertificateDer<'_>,
            _: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            rustls::crypto::ring::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }

    /// Let's Encrypt's side of TLS-ALPN-01: asks for `acme-tls/1` and
    /// `name`, and gives the certificate shown, if the handshake went.
    async fn as_lets_encrypt(certs: Arc<Certs>, name: &str, alpn: &[u8]) -> Option<Vec<u8>> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(vibe_mcp::public::server_config(certs));
        let server = tokio::spawn(async move {
            let (s, _) = listener.accept().await.unwrap();
            let tls = acceptor.accept(s).await.ok()?;
            tls.get_ref().1.alpn_protocol().map(<[u8]>::to_vec)
        });
        let recorder = Arc::new(Recorder::default());
        let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(recorder.clone())
        .with_no_client_auth();
        config.alpn_protocols = vec![alpn.to_vec()];
        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let name = rustls::pki_types::ServerName::try_from(name.to_owned()).unwrap();
        let ok = tokio_rustls::TlsConnector::from(Arc::new(config))
            .connect(name, tcp)
            .await
            .is_ok();
        let negotiated = server.await.unwrap();
        if !ok {
            return None;
        }
        assert_eq!(negotiated.as_deref(), Some(alpn), "the protocol it agreed");
        recorder.0.lock().unwrap().clone()
    }

    #[tokio::test]
    async fn lets_encrypt_is_shown_the_challenge_and_clients_the_certificate() {
        let certs = Arc::new(Certs::new("vps.example.com"));
        let (chain, key) = self_signed("vps.example.com");
        let real = load_pem(chain.as_bytes(), key.as_bytes()).unwrap();
        let real_der = real.key.cert[0].as_ref().to_vec();
        certs.set(real);
        // No challenge pending: an acme-tls/1 handshake gets nothing.
        assert_eq!(
            as_lets_encrypt(Arc::clone(&certs), "vps.example.com", ACME_TLS_ALPN).await,
            None
        );
        let challenge = challenge_cert("vps.example.com", &[7; 32]).unwrap();
        let challenge_der = challenge.cert[0].as_ref().to_vec();
        certs.set_challenge(Some(challenge));
        let shown = as_lets_encrypt(Arc::clone(&certs), "vps.example.com", ACME_TLS_ALPN).await;
        assert_eq!(
            shown,
            Some(challenge_der),
            "the challenge, to Let's Encrypt"
        );
        // Meanwhile, a client gets the real certificate over HTTP/1.1.
        let shown = as_lets_encrypt(Arc::clone(&certs), "vps.example.com", b"http/1.1").await;
        assert_eq!(shown, Some(real_der));
        // And the challenge only for its own name.
        assert_eq!(
            as_lets_encrypt(certs, "other.example.com", ACME_TLS_ALPN).await,
            None
        );
    }

    #[test]
    fn certificates_are_kept_privately() {
        let dir = tempfile::tempdir().unwrap();
        let disk = Disk::new(dir.path().join("external"));
        assert!(disk.load("vps.example.com").is_none());
        let (chain, key) = self_signed("vps.example.com");
        disk.save("vps.example.com", &chain, &key).unwrap();
        let l = disk.load("vps.example.com").unwrap().unwrap();
        assert!(l.validity.not_after > l.validity.not_before);
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir.path().join("external")), 0o700);
        assert_eq!(mode(&disk.key_path("vps.example.com")), 0o600);
        assert_eq!(mode(&disk.chain_path("vps.example.com")), 0o600);
        assert_ne!(
            disk.account_path(LETS_ENCRYPT),
            disk.account_path("https://acme-staging-v02.api.letsencrypt.org/directory")
        );
    }
}
