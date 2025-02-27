// Copyright (c) 2018-2022 The MobileCoin Foundation

//! A `grpcio::ServerCredentialsFetcher` implementation that reloads a GRPC's
//! server TLS certificate/key when a SIGHUP is received.

use displaydoc::Display;
use grpcio::{CertificateRequestType, ServerCredentialsBuilder, ServerCredentialsFetcher};
use mc_common::logger::{log, Logger};
use signal_hook::{consts::SIGHUP, flag};
use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

/// The `grpcio::ServerCredentialsFetcher` demands a root certificate for
/// verifying client identities, even when we explicitly specify
/// DontRequestClientCertificate. As such, we need to provide it with some
/// certificate. We don't use client certificate authentication, so we use a
/// hardcoded certificate that was generated by this command:
/// openssl req \
///     -x509 \
///     -newkey rsa:4096 \
///     -keyout server1.key \
///     -out server1.crt \
///     -days 3650 \
///     -subj "/C=US/ST=Neverland/L=California/O=Company Name/OU=Org" \
///     -nodes
const HARDCODED_CLIENT_ROOT_CERT: &str = r#"-----BEGIN CERTIFICATE-----
MIIFMjCCAxoCCQCQduVhGiJHZjANBgkqhkiG9w0BAQsFADBbMQswCQYDVQQGEwJV
UzESMBAGA1UECAwJTmV2ZXJsYW5kMRMwEQYDVQQHDApDYWxpZm9ybmlhMRUwEwYD
VQQKDAxDb21wYW55IE5hbWUxDDAKBgNVBAsMA09yZzAeFw0yMTAyMjIyMDQ0NTFa
Fw0zMTAyMjAyMDQ0NTFaMFsxCzAJBgNVBAYTAlVTMRIwEAYDVQQIDAlOZXZlcmxh
bmQxEzARBgNVBAcMCkNhbGlmb3JuaWExFTATBgNVBAoMDENvbXBhbnkgTmFtZTEM
MAoGA1UECwwDT3JnMIICIjANBgkqhkiG9w0BAQEFAAOCAg8AMIICCgKCAgEA1mkN
OoKwvT2JBGUCVE//C2JGLqxN+5QB2qtvM70qzal1DTNATMM5edOYxIx5hqXk6ivY
Jo3jVrVpgm/h01rBbuq8JhnEub8+UKRnbu7IKFTOE2UzExNPKvd7fMjKqdHM/b/m
ZpTz3TINLBx0veTrtXzN3xblq+Wa/tEKTBnMxvBJ0vdcB7kFsV+giNArmZeDAW+S
qhW+GAK783LAeSrrnVh4H+ZfYuxt+O3DzciRy5zafUTUALUJllWFGkcbA/GRC/9y
rOpEucpRCwvH3NuvqIvHTpWP4mRKFWmN1w9vftJ+g1JpipNsP2veUhPHvM/aCDRT
mAtkL37/i0SPAExDhVBh595e27nG49pyLQxDrj+d1TRGXDbc+nRQjvaTtyGs4DEg
V1Fm96Ltu1vX1x9gYMci3cXwzNrs0vfX2qwXgm25w6+5OJRBV3B02sPo44dN0oUO
Oybqtx6jUMzoudjcyBSwEqtKOhRw0lZV9TOo+vnBzQpymv49t1PezlK2WMqu/3Fi
6qVTHZwuPJNqojAyAWxink8w+7TewjCbqYenQI8qqJHcVVzilbhe1qOaT+Cjnnq+
xvN3tJIZKbzgVnpB3MKdqF6KKcBQWF3bHRoChv7zs7EvytUYwimqIAwOxDHLaHUg
J9swE4M3jgoY+6ZfGzj1Muyk9GJ5yNUjbjsQCMsCAwEAATANBgkqhkiG9w0BAQsF
AAOCAgEAHqaedXS09ZDzUh3LVimOwf/eg98ksSEN/5ANgTaFWDNBTT6go5YHKJXY
vDfyklxRLbi17IX3MDJRQw6GN5RUIhTZKKKhUiRE/sOJ7l4uXjcmYwV/iVqZgDL9
S1JMhQtytWxKDj/oaw4W9hK8xMIAQuRLrK9IzvvhJLFB8p8vvWabXtLEzVr9v2x8
Hf4zbHNW7W7eNOiLM7x/uyrWKfecO9LLjtk/rRyFr+enn3VWL4FmY2xyWsGGevXE
djwjdnZOCKOgsXMx8lHLeyItiSTuJFrjSyRjgzPudVJIjf2TV/ymvZHcY/IpRBOR
EUoqB0CtWYX15o+rVpXpRIjr0htmz7lbVDXHpNs+4oTBrc7VgoFbUTtsMJ5Kh42C
ioRMdWcs2D/7OOYZCZ7PAGClUwN0SxJ+lI1j6a9Fx5a42XFr4shVSrcgA7nsZBKq
VL/BUoJzU76GcrZAeDgyHLqgXARTVK8d4Gv63EIopAjfg8iS3JwSivpraQPW+IVN
G72MtHe8GLWAZIK/xvN9rVxnljPGGE6haIYWX2TBQDj+38gqfmckXVy72Vo1e1oV
Ot9sOqRdWgMdWsiqJpUVKE2ziZ7+Y1KWgK+qCEv2GD3GL6uR+LNChGPzyp1NHZmo
3xvG2wKtArm3GD5jm0Mx/Z1ej6FdXFrjtaXE5X+gZ9wTJfqsp4I=
-----END CERTIFICATE-----"#;

/// Certificate Reloader error.
#[derive(Debug, Display)]
pub enum ServerCertReloaderError {
    /// IO: {0}
    IO(io::Error),
}

impl From<io::Error> for ServerCertReloaderError {
    fn from(src: io::Error) -> Self {
        Self::IO(src)
    }
}

/// A `grpcio::ServerCredentialsFetcher` implementation that reloads a GRPC's
/// server TLS certificate/key when a SIGHUP is received.
pub struct ServerCertReloader {
    /// Certificate file to watch.
    cert_file: PathBuf,

    /// Private key file to watch.
    key_file: PathBuf,

    /// Signal that we need to re-load the certificate/key files.
    load_needed: Arc<AtomicBool>,

    /// Logger.
    logger: Logger,
}

impl ServerCertReloader {
    /// Create a new ServerCertReloader that watches `cert_file`/`key_file`.
    pub fn new(
        cert_file: &impl AsRef<Path>,
        key_file: &impl AsRef<Path>,
        logger: Logger,
    ) -> Result<Self, ServerCertReloaderError> {
        let load_needed = Arc::new(AtomicBool::new(true));

        flag::register(SIGHUP, load_needed.clone())?;

        Ok(Self {
            cert_file: cert_file.as_ref().to_path_buf(),
            key_file: key_file.as_ref().to_path_buf(),
            load_needed,
            logger,
        })
    }
}

impl ServerCredentialsFetcher for ServerCertReloader {
    fn fetch(&self) -> Result<Option<ServerCredentialsBuilder>, Box<dyn std::error::Error>> {
        if !self.load_needed.load(Ordering::SeqCst) {
            return Ok(None);
        }

        log::info!(self.logger, "Loading certificates");

        let crt = fs::read_to_string(&self.cert_file)?;
        let key = fs::read_to_string(&self.key_file)?;

        let new_cred = ServerCredentialsBuilder::new()
            // This sets the client root certificate to verify client's identity.
            // We are not using this feature, however grpcio still requires something to be set
            // there when using the ServerCredentialsFetcher mechanism. As a workaround we are
            // using the server's certificate chain here.
            .root_cert(
                HARDCODED_CLIENT_ROOT_CERT.as_bytes().to_vec(),
                CertificateRequestType::DontRequestClientCertificate,
            )
            .add_cert(crt.into(), key.into());

        self.load_needed.store(false, Ordering::SeqCst);
        Ok(Some(new_cred))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        health_api::PingRequest, health_api_grpc::HealthClient, ConnectionUriGrpcioServer,
        HealthService,
    };
    use grpcio::{ChannelBuilder, ChannelCredentialsBuilder, EnvBuilder, Server, ServerBuilder};
    use mc_common::logger::test_with_logger;
    use mc_crypto_x509_test_vectors::{ok_self_signed_1, ok_self_signed_2};
    use mc_util_uri::ConsensusClientUri;
    use std::{str::FromStr, thread, time::Duration};

    fn create_test_server(
        cert_file: &impl AsRef<Path>,
        key_file: &impl AsRef<Path>,
        logger: Logger,
    ) -> (Server, u16) {
        let env = Arc::new(EnvBuilder::new().build());
        let service = HealthService::new(None, logger.clone()).into_service();

        let mut server = ServerBuilder::new(env)
            .register_service(service)
            .bind_with_fetcher(
                "localhost",
                0,
                Box::new(ServerCertReloader::new(&cert_file, &key_file, logger.clone()).unwrap()),
                CertificateRequestType::DontRequestClientCertificate,
            )
            .build()
            .unwrap();
        server.start();
        let port = server.bind_addrs().next().unwrap().1;

        log::info!(logger, "Server started on port {}", port);

        (server, port)
    }

    fn create_test_client(cert: &str, ssl_target: &str, port: u16) -> HealthClient {
        let env = Arc::new(EnvBuilder::new().build());
        let cred = ChannelCredentialsBuilder::new()
            .root_cert(cert.into())
            .build();
        let ch = ChannelBuilder::new(env)
            .override_ssl_target(ssl_target)
            .secure_connect(&format!("localhost:{}", port), cred);
        HealthClient::new(ch)
    }

    #[test_with_logger]
    fn test_cert_reloading(logger: Logger) {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let cert_file = temp_dir.path().join("server.crt");
        let key_file = temp_dir.path().join("server.key");

        // Load test certs and keys
        let (server1_cert, server1_key) = ok_self_signed_1();
        let (server2_cert, server2_key) = ok_self_signed_2();

        // Write server1's cert files into the temp dir.
        std::fs::write(&cert_file, &server1_cert).unwrap();
        std::fs::write(&key_file, &server1_key).unwrap();

        // Start the GRPC server.
        let (_server, port) = create_test_server(&cert_file, &key_file, logger.clone());

        // Connect the server whose CN is "www.server1.com" with the correct
        // certificate.
        let client1 = create_test_client(&server1_cert, "www.server1.com", port);
        let mut req = PingRequest::default();
        req.set_data(vec![1, 2, 3]);
        let reply = client1.ping(&req).expect("rpc");
        assert_eq!(reply.get_data(), vec![1, 2, 3]);

        // Connect the server whose CN is "www.server1.com" with a different ssl target
        // should fail.
        let client2 = create_test_client(&server1_cert, "www.server2.com", port);
        let mut req = PingRequest::default();
        req.set_data(vec![1, 2, 3]);
        assert!(client2.ping(&req).is_err());

        // Connect the server whose CN is "www.server1.com" with an incorrect
        // certificate.
        let client3 = create_test_client(&server2_cert, "www.server1.com", port);
        let mut req = PingRequest::default();
        req.set_data(vec![1, 2, 3]);
        assert!(client3.ping(&req).is_err());

        // Connecting with server2/"www.server2.com" should not work until we replace
        // the certificate and key file.
        let client4 = create_test_client(&server2_cert, "www.server2.com", port);
        let mut req = PingRequest::default();
        req.set_data(vec![1, 2, 3]);
        assert!(client4.ping(&req).is_err());

        // Replace server1 certificates with server2. This should trigger the reloading
        // mechanism.
        std::fs::write(&cert_file, &server2_cert).unwrap();
        std::fs::write(&key_file, &server2_key).unwrap();

        // Trigger reloading.
        unsafe {
            libc::kill(libc::getpid(), libc::SIGHUP);
        }

        // Give the reloader time to pick up the changes.
        thread::sleep(Duration::from_secs(2));

        // We should be able to connect using "www.server2.com".
        let client5 = create_test_client(&server2_cert, "www.server2.com", port);
        let mut req = PingRequest::default();
        req.set_data(vec![1, 2, 3]);
        let reply = client5.ping(&req).expect("rpc");
        assert_eq!(reply.get_data(), vec![1, 2, 3]);

        // The original client should still be functional.
        req.set_data(vec![5, 6, 7]);
        let reply = client1.ping(&req).expect("rpc");
        assert_eq!(reply.get_data(), vec![5, 6, 7]);

        // The previous server2 client should also work.
        let mut req = PingRequest::default();
        req.set_data(vec![1, 2, 3]);
        let reply = client4.ping(&req).expect("rpc");
        assert_eq!(reply.get_data(), vec![1, 2, 3]);
    }

    #[test_with_logger]
    fn test_reload_invalid_data(logger: Logger) {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let cert_file = temp_dir.path().join("server.crt");
        let key_file = temp_dir.path().join("server.key");

        // Load test certs and keys
        let (server1_cert, server1_key) = ok_self_signed_1();

        // Write server1's cert files into the temp dir.
        std::fs::write(&cert_file, &server1_cert).unwrap();
        std::fs::write(&key_file, &server1_key).unwrap();

        // Start the GRPC server.
        let (_server, port) = create_test_server(&cert_file, &key_file, logger.clone());

        // Sanity that the server works.
        let client1 = create_test_client(&server1_cert, "www.server1.com", port);
        let mut req = PingRequest::default();
        req.set_data(vec![1, 2, 3]);
        let reply = client1.ping(&req).expect("rpc");
        assert_eq!(reply.get_data(), vec![1, 2, 3]);

        // Replace the certificate file with junk.
        fs::write(cert_file, "junk").unwrap();

        // Trigger reloading.
        unsafe {
            libc::kill(libc::getpid(), libc::SIGHUP);
        }

        // Give the reloader time to pick up the changes.
        thread::sleep(Duration::from_secs(2));

        // Server should still respond with the old certificate.
        let client2 = create_test_client(&server1_cert, "www.server1.com", port);
        let mut req = PingRequest::default();
        req.set_data(vec![1, 2, 3]);
        let reply = client2.ping(&req).expect("rpc");
        assert_eq!(reply.get_data(), vec![1, 2, 3]);
    }

    #[test_with_logger]
    fn test_multiple_servers(logger: Logger) {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let cert_file = temp_dir.path().join("server.crt");
        let key_file = temp_dir.path().join("server.key");

        // Load test certs and keys
        let (server1_cert, server1_key) = ok_self_signed_1();
        let (server2_cert, server2_key) = ok_self_signed_2();

        // Write server1's cert files into the temp dir.
        std::fs::write(&cert_file, &server1_cert).unwrap();
        std::fs::write(&key_file, &server1_key).unwrap();

        // Start the GRPC servers.
        let (_server1, port1) = create_test_server(&cert_file, &key_file, logger.clone());
        let (_server2, port2) = create_test_server(&cert_file, &key_file, logger.clone());

        // Sanity that the servers works.
        let client1 = create_test_client(&server1_cert, "www.server1.com", port1);
        let mut req = PingRequest::default();
        req.set_data(vec![1, 2, 3]);
        let reply = client1.ping(&req).expect("rpc");
        assert_eq!(reply.get_data(), vec![1, 2, 3]);

        let client2 = create_test_client(&server1_cert, "www.server1.com", port2);
        let mut req = PingRequest::default();
        req.set_data(vec![1, 2, 3]);
        let reply = client2.ping(&req).expect("rpc");
        assert_eq!(reply.get_data(), vec![1, 2, 3]);

        // Replace server1 certificates with server2.
        std::fs::write(&cert_file, &server2_cert).unwrap();
        std::fs::write(&key_file, &server2_key).unwrap();

        // Trigger reloading.
        unsafe {
            libc::kill(libc::getpid(), libc::SIGHUP);
        }

        // Give the reloader time to pick up the changes.
        thread::sleep(Duration::from_secs(2));

        // Both servers should now have the new cerficates.
        let client3 = create_test_client(&server2_cert, "www.server2.com", port1);
        let mut req = PingRequest::default();
        req.set_data(vec![1, 2, 3]);
        let reply = client3.ping(&req).expect("rpc");
        assert_eq!(reply.get_data(), vec![1, 2, 3]);

        let client4 = create_test_client(&server2_cert, "www.server2.com", port2);
        let mut req = PingRequest::default();
        req.set_data(vec![1, 2, 3]);
        let reply = client4.ping(&req).expect("rpc");
        assert_eq!(reply.get_data(), vec![1, 2, 3]);
    }

    #[test_with_logger]
    fn test_bind_using_uri(logger: Logger) {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let cert_file = temp_dir.path().join("server.crt");
        let key_file = temp_dir.path().join("server.key");

        // Load test certs and keys
        let (server1_cert, server1_key) = ok_self_signed_1();
        let (server2_cert, server2_key) = ok_self_signed_2();

        // Write server1's cert files into the temp dir.
        std::fs::write(&cert_file, &server1_cert).unwrap();
        std::fs::write(&key_file, &server1_key).unwrap();

        // Create a listener URI.
        let port: u16 = 6544;
        let uri = ConsensusClientUri::from_str(&format!(
            "mc://localhost:{}/?tls-chain={}&tls-key={}",
            port,
            cert_file.clone().into_os_string().into_string().unwrap(),
            key_file.clone().into_os_string().into_string().unwrap()
        ))
        .unwrap();

        // Start server using bind_using_uri.
        let env = Arc::new(EnvBuilder::new().build());
        let service = HealthService::new(None, logger.clone()).into_service();

        let mut server = ServerBuilder::new(env)
            .register_service(service)
            .bind_using_uri(&uri, logger.clone())
            .build()
            .unwrap();
        server.start();

        // Sanity that the servers works.
        let client1 = create_test_client(&server1_cert, "www.server1.com", port);
        let mut req = PingRequest::default();
        req.set_data(vec![1, 2, 3]);
        let reply = client1.ping(&req).expect("rpc");
        assert_eq!(reply.get_data(), vec![1, 2, 3]);

        // Replace server1 certificates with server2.
        std::fs::write(&cert_file, &server2_cert).unwrap();
        std::fs::write(&key_file, &server2_key).unwrap();

        // Trigger reloading.
        unsafe {
            libc::kill(libc::getpid(), libc::SIGHUP);
        }

        // Give the reloader time to pick up the changes.
        thread::sleep(Duration::from_secs(2));

        // Server should now have the new cerficate.
        let client2 = create_test_client(&server2_cert, "www.server2.com", port);
        let mut req = PingRequest::default();
        req.set_data(vec![1, 2, 3]);
        let reply = client2.ping(&req).expect("rpc");
        assert_eq!(reply.get_data(), vec![1, 2, 3]);

        // Original client should still be alive.
        let mut req = PingRequest::default();
        req.set_data(vec![1, 2, 3]);
        let reply = client1.ping(&req).expect("rpc");
        assert_eq!(reply.get_data(), vec![1, 2, 3]);
    }
}
