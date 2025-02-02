use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use clap::*;
use dnsclient::sync::DNSClient;
use lazy_static::lazy_static;
use pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

use crate::repository::access_repo::in_memory_repo::InMemAccessRepo;
use crate::repository::access_repo::AccessRepository;
use crate::repository::service_repo::in_memory_repo::InMemServiceRepo;
use crate::repository::service_repo::ServiceRepository;
use crate::repository::user_repo::in_memory_repo::InMemUserRepo;
use crate::repository::user_repo::UserRepository;
use regex::Regex;
use rustls::crypto::CryptoProvider;
use rustls::server::danger::ClientCertVerifier;
use rustls::server::WebPkiClientVerifier;
use trust0_common::crypto::alpn;
use trust0_common::crypto::file::CRLFile;
use trust0_common::crypto::file::{load_certificates, load_private_key};
use trust0_common::error::AppError;

/// Client response messages
pub const RESPCODE_0403_FORBIDDEN: u16 = 403;
pub const RESPCODE_0420_INVALID_CLIENT_CERTIFICATE: u16 = 420;
pub const RESPCODE_0421_UNKNOWN_USER: u16 = 421;
pub const RESPCODE_0422_INACTIVE_USER: u16 = 422;
pub const RESPCODE_0423_INVALID_REQUEST: u16 = 423;
pub const RESPCODE_0424_INVALID_ALPN_PROTOCOL: u16 = 424;
pub const RESPCODE_0425_INACTIVE_SERVICE_PROXY: u16 = 425;
pub const RESPCODE_0500_SYSTEM_ERROR: u16 = 500;
pub const RESPCODE_0520_UNKNOWN_CODE: u16 = 520;
const RESPMSG_0403_FORBIDDEN: &str = "[E0403] Access is forbidden";
const RESPMSG_0420_INVALID_CLIENT_CERTIFICATE: &str = "[E0420] Invalid client certificate";
const RESPMSG_0421_UNKNOWN_USER: &str = "[E0421] Unknown user is inactive";
const RESPMSG_0422_INACTIVE_USER: &str = "[E0422] User account is inactive";
const RESPMSG_0423_INVALID_REQUEST: &str = "[E0423] Invalid request";
const RESPMSG_0424_INVALID_ALPN_PROTOCOL: &str = "[E0424] Invalid ALPN protocol";
const RESPMSG_0425_INACTIVE_SERVICE_PROXY: &str = "[E0425] Inactive service proxy";
const RESPMSG_0500_SYSTEM_ERROR: &str = "[E0500] System error occurred";
const RESPMSG_0520_UNKNOWN_CODE: &str = "[E0520] System error occurred";

lazy_static! {
    pub static ref RESPONSE_MSGS: HashMap<u16, &'static str> = {
        HashMap::from([
            (RESPCODE_0403_FORBIDDEN, RESPMSG_0403_FORBIDDEN),
            (
                RESPCODE_0420_INVALID_CLIENT_CERTIFICATE,
                RESPMSG_0420_INVALID_CLIENT_CERTIFICATE,
            ),
            (RESPCODE_0421_UNKNOWN_USER, RESPMSG_0421_UNKNOWN_USER),
            (RESPCODE_0422_INACTIVE_USER, RESPMSG_0422_INACTIVE_USER),
            (RESPCODE_0423_INVALID_REQUEST, RESPMSG_0423_INVALID_REQUEST),
            (
                RESPCODE_0424_INVALID_ALPN_PROTOCOL,
                RESPMSG_0424_INVALID_ALPN_PROTOCOL,
            ),
            (
                RESPCODE_0425_INACTIVE_SERVICE_PROXY,
                RESPMSG_0425_INACTIVE_SERVICE_PROXY,
            ),
            (RESPCODE_0500_SYSTEM_ERROR, RESPMSG_0500_SYSTEM_ERROR),
            (RESPCODE_0520_UNKNOWN_CODE, RESPMSG_0520_UNKNOWN_CODE),
        ])
    };
}

/// Which mode the server operates in.
#[derive(Copy, Clone, Default, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
pub enum ServerMode {
    /// Control-plane for service gateway management
    #[default]
    ControlPlane,

    /// Forward traffic to respective service
    Proxy,
}

/// Datasource configuration for the trust framework entities
#[derive(Subcommand, Debug, Clone)]
pub enum DataSource {
    /// No DB configured, used in testing
    NoDB,

    /// In-memory DB, with a simple backing persistence store
    InMemoryDb(InMemoryDb),
}

impl DataSource {
    #[allow(clippy::type_complexity)]
    /// Return tuple of repository factory closures (respectively for access, service and user repositories)
    pub fn repository_factories(
        &self,
    ) -> (
        Box<dyn Fn() -> Arc<Mutex<dyn AccessRepository>>>,
        Box<dyn Fn() -> Arc<Mutex<dyn ServiceRepository>>>,
        Box<dyn Fn() -> Arc<Mutex<dyn UserRepository>>>,
    ) {
        (
            Box::new(|| Arc::new(Mutex::new(InMemAccessRepo::new()))),
            Box::new(|| Arc::new(Mutex::new(InMemServiceRepo::new()))),
            Box::new(|| Arc::new(Mutex::new(InMemUserRepo::new()))),
        )
    }
}

#[derive(Args, Debug, Clone)]
pub struct InMemoryDb {
    /// (Service) Access entity store JSON file path
    #[arg(required = true, short = 'a', long = "access-db-file", env)]
    pub access_db_file: String,

    /// Service entity store JSON file path
    #[arg(required = true, short = 's', long = "service-db-file", env)]
    pub service_db_file: String,

    /// User entity store JSON file path
    #[arg(required = true, short = 'u', long = "user-db-file", env)]
    pub user_db_file: String,
}

/// Runs a trust0 gateway server on :PORT.  The default PORT is 443.
#[derive(Parser)]
#[command(author, version, long_about)]
pub struct AppConfigArgs {
    /// Listen on PORT
    #[arg(
        required = true,
        short = 'p',
        long = "port",
        env,
        default_value_t = 443
    )]
    pub port: u16,

    /// Read server certificates from <CERT_FILE>. This should contain PEM-format certificates
    /// in the right order (first certificate should certify <KEY_FILE>, last should be a root CA)
    #[arg(required=true, short='c', long="cert-file", env, value_parser=trust0_common::crypto::file::verify_certificates)]
    pub cert_file: String,

    /// Read private key from <KEY_FILE>.  This should be a RSA private key or PKCS8-encoded
    /// private key, in PEM format
    #[arg(required=true, short='k', long="key-file", env, value_parser=trust0_common::crypto::file::verify_private_key_file)]
    pub key_file: String,

    /// Accept client authentication certificates signed by those roots provided in <AUTH_CERT_FILE>
    #[arg(required=true, short='a', long="auth-cert-file", env, value_parser=trust0_common::crypto::file::verify_certificates)]
    pub auth_cert_file: String,

    /// EXPERIMENTAL. Perform client certificate revocation checking using the DER-encoded <CRL_FILE(s)>. Will update list during runtime, if file has changed.
    #[cfg(feature = "experimental-crl")]
    #[arg(required=false, long="crl-file", env, value_parser=trust0_common::crypto::file::verify_crl_list)]
    pub crl_file: Option<String>,
    #[cfg(not(feature = "experimental-crl"))]
    #[arg(skip=None)]
    pub crl_file: Option<String>,

    /// Disable default TLS version list, and use <PROTOCOL_VERSION(s)> instead
    #[arg(required=false, long="protocol-version", env, value_parser=trust0_common::crypto::tls::lookup_version)]
    pub protocol_version: Option<Vec<&'static rustls::SupportedProtocolVersion>>,

    /// Disable default cipher suite list, and use <CIPHER_SUITE(s)> instead
    #[arg(required=false, long="cipher-suite", env, value_parser=trust0_common::crypto::tls::lookup_suite)]
    pub cipher_suite: Option<Vec<rustls::SupportedCipherSuite>>,

    /// Negotiate ALPN using <ALPN_PROTOCOL(s)>
    #[arg(required=false, long="alpn-protocol", env, value_parser=trust0_common::crypto::tls::parse_alpn_protocol)]
    pub alpn_protocol: Option<Vec<Vec<u8>>>,

    /// Support session resumption
    #[arg(required = false, long = "session-resumption", env)]
    pub session_resumption: bool,

    /// Support tickets
    #[arg(required = false, long = "tickets", env)]
    pub tickets: bool,

    /// Hostname/ip of this gateway given to clients, used in service proxy connections (if not supplied, clients will determine that on their own)
    #[arg(required = true, long = "gateway-service-host", env)]
    pub gateway_service_host: Option<String>,

    /// Service proxy port range. If this is omitted, service connections can be made to the primary gateway port (in addition to the control plane connection). ALPN protocol configuration is used to specify the service ID.
    #[arg(required=false, long="gateway-service-ports", env, value_parser=crate::config::AppConfig::parse_gateway_service_ports)]
    pub gateway_service_ports: Option<(u16, u16)>,

    /// Hostname/ip of this gateway, which is routable by UDP services, used in UDP socket replies. If not supplied, then "127.0.0.1" will be used (if necessary)
    #[arg(required = false, long = "gateway-service-reply-host", env)]
    pub gateway_service_reply_host: Option<String>,

    /// Enable verbose logging
    #[arg(required = false, long = "verbose", env)]
    pub verbose: bool,

    /// Show all gateway and service addresses (in REPL shell responses)
    #[arg(required = false, long = "no-mask-addrs", default_value_t = false, env)]
    pub no_mask_addresses: bool,

    /// Server mode: startup server as control-plane, or as a stand-alone service gateway node
    #[arg(required = false, value_enum, long = "mode", env)]
    pub mode: Option<ServerMode>,

    /// DB datasource configuration
    #[command(subcommand)]
    pub datasource: DataSource,
}

/// TLS server configuration builder
pub struct TlsServerConfigBuilder {
    pub certs: Vec<CertificateDer<'static>>,
    pub key: PrivateKeyDer<'static>,
    pub cipher_suites: Vec<rustls::SupportedCipherSuite>,
    pub protocol_versions: Vec<&'static rustls::SupportedProtocolVersion>,
    pub auth_root_certs: rustls::RootCertStore,
    pub crl_file: Option<Arc<Mutex<CRLFile>>>,
    pub session_resumption: bool,
    pub alpn_protocols: Vec<Vec<u8>>,
}

impl TlsServerConfigBuilder {
    /// Create TLS server configuration
    pub fn build(&self) -> Result<rustls::ServerConfig, AppError> {
        let mut tls_server_config = rustls::ServerConfig::builder_with_provider(
            CryptoProvider {
                cipher_suites: self.cipher_suites.to_vec(),
                ..rustls::crypto::ring::default_provider()
            }
            .into(),
        )
        .with_protocol_versions(self.protocol_versions.as_slice())
        .expect("inconsistent cipher-suites/versions specified")
        .with_client_cert_verifier(self.build_client_cert_verifier()?)
        .with_single_cert(
            self.certs.clone(),
            PrivatePkcs8KeyDer::from(self.key.secret_der().to_owned()).into(),
        )
        .expect("bad certificates/private key");

        tls_server_config.key_log = Arc::new(rustls::KeyLogFile::new());

        if self.session_resumption {
            tls_server_config.session_storage = rustls::server::ServerSessionMemoryCache::new(256);
        }

        tls_server_config.alpn_protocols = self.alpn_protocols.clone();

        Ok(tls_server_config)
    }

    /// Build a TLS client verifier
    #[cfg(feature = "experimental-crl")]
    fn build_client_cert_verifier(&self) -> Result<Arc<dyn ClientCertVerifier>, AppError> {
        let crl_list = match &self.crl_file {
            Some(crl_file) => vec![crl_file.lock().unwrap().crl_list()?],
            None => vec![],
        };

        Ok(
            WebPkiClientVerifier::builder(Arc::new(self.auth_root_certs.clone()))
                .with_crls(crl_list)
                .build()
                .unwrap(),
        )
    }
    #[cfg(not(feature = "experimental-crl"))]
    fn build_client_cert_verifier(&self) -> Result<Arc<dyn ClientCertVerifier>, AppError> {
        Ok(
            WebPkiClientVerifier::builder(Arc::new(self.auth_root_certs.clone()))
                .build()
                .unwrap(),
        )
    }
}

/// Main application configuration/context struct
pub struct AppConfig {
    pub server_mode: ServerMode,
    pub server_port: u16,
    pub tls_server_config_builder: TlsServerConfigBuilder,
    pub verbose_logging: bool,
    pub access_repo: Arc<Mutex<dyn AccessRepository>>,
    pub service_repo: Arc<Mutex<dyn ServiceRepository>>,
    pub user_repo: Arc<Mutex<dyn UserRepository>>,
    pub gateway_service_host: Option<String>,
    pub gateway_service_ports: Option<(u16, u16)>,
    pub gateway_service_reply_host: String,
    pub mask_addresses: bool,
    pub dns_client: DNSClient,
}

impl AppConfig {
    /// Load config
    pub fn new() -> Result<Self, AppError> {
        // parse process arguments

        let config_args = AppConfigArgs::parse();

        // Datasource repositories

        let repositories = Self::create_datasource_repositories(
            &config_args.datasource,
            &config_args.datasource.repository_factories(),
        )?;

        // create TLS server configuration builder

        let auth_certs = load_certificates(config_args.auth_cert_file.clone()).unwrap();
        let certs = load_certificates(config_args.cert_file.clone()).unwrap();
        let key = load_private_key(config_args.key_file.clone()).unwrap();

        let crl_file = if cfg!(feature = "experimental-crl") {
            match &config_args.crl_file {
                Some(filepath) => {
                    let crl_file = CRLFile::new(filepath.as_str());
                    crl_file.spawn_list_reloader(
                        None,
                        Some(Box::new(|err| {
                            panic!("Error during CRL reload, exiting: err={:?}", &err);
                        })),
                    );
                    Some(Arc::new(Mutex::new(crl_file)))
                }
                None => None,
            }
        } else {
            None
        };

        let mut auth_root_certs = rustls::RootCertStore::empty();
        for auth_root_cert in auth_certs {
            auth_root_certs.add(auth_root_cert).unwrap();
        }

        let cipher_suites: Vec<rustls::SupportedCipherSuite> = config_args
            .cipher_suite
            .unwrap_or(rustls::crypto::ring::ALL_CIPHER_SUITES.to_vec());
        let protocol_versions: Vec<&'static rustls::SupportedProtocolVersion> = config_args
            .protocol_version
            .unwrap_or(rustls::ALL_VERSIONS.to_vec());
        let session_resumption = config_args.session_resumption;

        let mut alpn_protocols = vec![alpn::Protocol::ControlPlane.to_string().into_bytes()];
        for service in repositories.1.as_ref().lock().unwrap().get_all()? {
            alpn_protocols
                .push(alpn::Protocol::create_service_protocol(service.service_id).into_bytes())
        }

        let tls_server_config_builder = TlsServerConfigBuilder {
            certs,
            key,
            cipher_suites,
            protocol_versions,
            auth_root_certs,
            crl_file,
            session_resumption,
            alpn_protocols,
        };

        // Miscellaneous

        let dns_client = DNSClient::new_with_system_resolvers().map_err(|err| {
            AppError::GenWithMsgAndErr("Error instantiating DNSClient".to_string(), Box::new(err))
        })?;

        // Instantiate AppConfig

        Ok(AppConfig {
            server_mode: config_args.mode.unwrap_or_default(),
            server_port: config_args.port,
            tls_server_config_builder,
            verbose_logging: config_args.verbose,
            access_repo: repositories.0,
            service_repo: repositories.1,
            user_repo: repositories.2,
            gateway_service_host: config_args.gateway_service_host,
            gateway_service_ports: config_args.gateway_service_ports,
            gateway_service_reply_host: config_args
                .gateway_service_reply_host
                .unwrap_or("127.0.0.1".to_string()),
            mask_addresses: !config_args.no_mask_addresses,
            dns_client,
        })
    }

    #[allow(clippy::type_complexity)]
    /// Instantiate main repositories based on datasource config. Returns tuple of access, service and user repositories.
    fn create_datasource_repositories(
        datasource: &DataSource,
        repo_factories: &(
            Box<dyn Fn() -> Arc<Mutex<dyn AccessRepository>>>,
            Box<dyn Fn() -> Arc<Mutex<dyn ServiceRepository>>>,
            Box<dyn Fn() -> Arc<Mutex<dyn UserRepository>>>,
        ),
    ) -> Result<
        (
            Arc<Mutex<dyn AccessRepository>>,
            Arc<Mutex<dyn ServiceRepository>>,
            Arc<Mutex<dyn UserRepository>>,
        ),
        AppError,
    > {
        let access_repository = repo_factories.0();
        let service_repository = repo_factories.1();
        let user_repository = repo_factories.2();

        if let DataSource::InMemoryDb(args) = datasource {
            access_repository
                .lock()
                .unwrap()
                .connect_to_datasource(&args.access_db_file)?;
            service_repository
                .lock()
                .unwrap()
                .connect_to_datasource(&args.service_db_file)?;
            user_repository
                .lock()
                .unwrap()
                .connect_to_datasource(&args.user_db_file)?;
        }

        Ok((access_repository, service_repository, user_repository))
    }

    /// Parse service port range (format "{port_start:u16}-{port_end:u16}")
    fn parse_gateway_service_ports(
        gateway_service_ports_str: &str,
    ) -> Result<(u16, u16), AppError> {
        let number_range_re = Regex::new(r"(\d+)-(\d+)").unwrap();

        let number_captures =
            number_range_re
                .captures(gateway_service_ports_str)
                .ok_or(AppError::General(format!(
                    "Invalid gateway service port range: val={}",
                    gateway_service_ports_str
                )))?;

        let port_start: u16 = number_captures[1].parse().unwrap_or(0);
        let port_end: u16 = number_captures[2].parse().unwrap_or(0);

        if (port_start == 0) || (port_end == 0) {
            return Err(AppError::General(format!(
                "Invalid gateway service port range (u16 vals required): val={}",
                gateway_service_ports_str
            )));
        }

        Ok((port_start, port_end))
    }
}

/// Unit tests
#[cfg(test)]
pub mod tests {

    use super::*;
    use crate::repository::access_repo::tests::MockAccessRepo;
    use crate::repository::access_repo::AccessRepository;
    use crate::repository::service_repo::tests::MockServiceRepo;
    use crate::repository::service_repo::ServiceRepository;
    use crate::repository::user_repo::tests::MockUserRepo;
    use crate::repository::user_repo::UserRepository;
    use mockall::predicate;
    use std::path::PathBuf;

    const CERTFILE_GATEWAY_PATHPARTS: [&str; 3] =
        [env!("CARGO_MANIFEST_DIR"), "testdata", "gateway.crt.pem"];
    const KEYFILE_GATEWAY_PATHPARTS: [&str; 3] =
        [env!("CARGO_MANIFEST_DIR"), "testdata", "gateway.key.pem"];

    // Utilities

    pub fn create_app_config_with_repos(
        user_repo: Arc<Mutex<dyn UserRepository>>,
        service_repo: Arc<Mutex<dyn ServiceRepository>>,
        access_repo: Arc<Mutex<dyn AccessRepository>>,
    ) -> Result<AppConfig, AppError> {
        let gateway_cert_file: PathBuf = CERTFILE_GATEWAY_PATHPARTS.iter().collect();
        let gateway_cert = load_certificates(gateway_cert_file.to_str().unwrap().to_string())?;
        let gateway_key_file: PathBuf = KEYFILE_GATEWAY_PATHPARTS.iter().collect();
        let gateway_key = load_private_key(gateway_key_file.to_str().unwrap().to_string())?;
        let auth_root_certs = rustls::RootCertStore::empty();
        let cipher_suites: Vec<rustls::SupportedCipherSuite> =
            rustls::crypto::ring::ALL_CIPHER_SUITES.to_vec();
        let protocol_versions: Vec<&'static rustls::SupportedProtocolVersion> =
            rustls::ALL_VERSIONS.to_vec();
        let session_resumption = false;
        let alpn_protocols = vec![alpn::Protocol::ControlPlane.to_string().into_bytes()];

        let tls_server_config_builder = TlsServerConfigBuilder {
            certs: gateway_cert,
            key: gateway_key,
            cipher_suites,
            protocol_versions,
            auth_root_certs,
            crl_file: None,
            session_resumption,
            alpn_protocols,
        };

        Ok(AppConfig {
            server_mode: ServerMode::ControlPlane,
            server_port: 2000,
            tls_server_config_builder,
            verbose_logging: false,
            access_repo,
            service_repo,
            user_repo,
            gateway_service_host: None,
            gateway_service_ports: None,
            gateway_service_reply_host: "".to_string(),
            mask_addresses: false,
            dns_client: DNSClient::new_with_system_resolvers().map_err(|err| {
                AppError::GenWithMsgAndErr(
                    "Error instantiating DNSClient".to_string(),
                    Box::new(err),
                )
            })?,
        })
    }

    #[test]
    pub fn appconfig_parse_gateway_service_ports_when_invalid_range() {
        if let Ok(range) = AppConfig::parse_gateway_service_ports("20-NAN") {
            panic!("Unexpected result: val={:?}", &range);
        }
    }

    #[test]
    pub fn appconfig_parse_gateway_service_ports_when_valid_range() {
        let result = AppConfig::parse_gateway_service_ports("20-40");
        if let Ok(range) = result {
            assert_eq!(range, (20, 40));
            return;
        }

        panic!("Unexpected result: val={:?}", &result);
    }

    #[test]
    pub fn appconfig_create_datasource_repositories_when_inmemdb_ds() {
        let repo_factories: (
            Box<dyn Fn() -> Arc<Mutex<dyn AccessRepository>>>,
            Box<dyn Fn() -> Arc<Mutex<dyn ServiceRepository>>>,
            Box<dyn Fn() -> Arc<Mutex<dyn UserRepository>>>,
        ) = (
            Box::new(move || {
                let mut access_repo = MockAccessRepo::new();
                access_repo
                    .expect_connect_to_datasource()
                    .with(predicate::eq("adf"))
                    .times(1)
                    .return_once(move |_| Ok(()));
                Arc::new(Mutex::new(access_repo))
            }),
            Box::new(move || {
                let mut service_repo = MockServiceRepo::new();
                service_repo
                    .expect_connect_to_datasource()
                    .with(predicate::eq("sdf"))
                    .times(1)
                    .return_once(move |_| Ok(()));
                Arc::new(Mutex::new(service_repo))
            }),
            Box::new(move || {
                let mut user_repo = MockUserRepo::new();
                user_repo
                    .expect_connect_to_datasource()
                    .with(predicate::eq("udf"))
                    .times(1)
                    .return_once(move |_| Ok(()));
                Arc::new(Mutex::new(user_repo))
            }),
        );

        let datasource = DataSource::InMemoryDb(InMemoryDb {
            access_db_file: "adf".to_string(),
            service_db_file: "sdf".to_string(),
            user_db_file: "udf".to_string(),
        });

        let result = AppConfig::create_datasource_repositories(&datasource, &repo_factories);

        if let Err(err) = &result {
            panic!("Unexpected result: err={:?}", err);
        }
    }

    #[test]
    pub fn appconfig_create_datasource_repositories_when_nodb_ds() {
        let repo_factories: (
            Box<dyn Fn() -> Arc<Mutex<dyn AccessRepository>>>,
            Box<dyn Fn() -> Arc<Mutex<dyn ServiceRepository>>>,
            Box<dyn Fn() -> Arc<Mutex<dyn UserRepository>>>,
        ) = (
            Box::new(move || {
                let mut access_repo = MockAccessRepo::new();
                access_repo.expect_connect_to_datasource().never();
                Arc::new(Mutex::new(access_repo))
            }),
            Box::new(move || {
                let mut service_repo = MockServiceRepo::new();
                service_repo.expect_connect_to_datasource().never();
                Arc::new(Mutex::new(service_repo))
            }),
            Box::new(move || {
                let mut user_repo = MockUserRepo::new();
                user_repo.expect_connect_to_datasource().never();
                Arc::new(Mutex::new(user_repo))
            }),
        );

        let datasource = DataSource::NoDB;

        let result = AppConfig::create_datasource_repositories(&datasource, &repo_factories);

        if let Err(err) = &result {
            panic!("Unexpected result: err={:?}", err);
        }
    }
}
