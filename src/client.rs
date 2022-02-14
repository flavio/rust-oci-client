//! OCI distribution client
//!
//! *Note*: This client is very feature poor. We hope to expand this to be a complete
//! OCI distribution client in the future.

use crate::errors::*;
use crate::manifest::{
    ImageIndexEntry, OciDescriptor, OciImageManifest, OciManifest, Versioned,
    IMAGE_CONFIG_MEDIA_TYPE, IMAGE_LAYER_GZIP_MEDIA_TYPE, IMAGE_LAYER_MEDIA_TYPE,
    IMAGE_MANIFEST_LIST_MEDIA_TYPE, IMAGE_MANIFEST_MEDIA_TYPE, OCI_IMAGE_INDEX_MEDIA_TYPE,
    OCI_IMAGE_MEDIA_TYPE,
};
use crate::secrets::RegistryAuth;
use crate::secrets::*;
use crate::Reference;

use crate::token_cache::{RegistryOperation, RegistryToken, RegistryTokenType, TokenCache};
use anyhow::{anyhow, Context};
use futures_util::future;
use futures_util::stream::StreamExt;
use hyperx::header::{Header, Raw};
use olpc_cjson::CanonicalFormatter;
use reqwest::header::HeaderMap;
use reqwest::{RequestBuilder, Url};
use serde::Serialize;
use sha2::Digest;
use std::collections::HashMap;
use std::convert::TryFrom;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tracing::{debug, trace, warn};
use www_authenticate::{Challenge, ChallengeFields, RawChallenge, WwwAuthenticate};

const MIME_TYPES_DISTRIBUTION_MANIFEST: &[&str] = &[
    IMAGE_MANIFEST_MEDIA_TYPE,
    IMAGE_MANIFEST_LIST_MEDIA_TYPE,
    OCI_IMAGE_MEDIA_TYPE,
    OCI_IMAGE_INDEX_MEDIA_TYPE,
];

/// The data for an image or module.
#[derive(Clone)]
pub struct ImageData {
    /// The layers of the image or module.
    pub layers: Vec<ImageLayer>,
    /// The digest of the image or module.
    pub digest: Option<String>,
    /// The Configuration object of the image or module.
    pub config: Config,
    /// The manifest of the image or module.
    pub manifest: Option<OciImageManifest>,
}

impl ImageData {
    /// Helper function to compute the digest of the image layers
    pub fn sha256_digest(&self) -> String {
        sha256_digest(
            &self
                .layers
                .iter()
                .cloned()
                .map(|l| l.data)
                .flatten()
                .collect::<Vec<u8>>(),
        )
    }

    /// Returns the image digest, either the value in the field or by computing it
    /// If the value in the field is None, the computed value will be stored
    pub fn digest(&self) -> String {
        self.digest.clone().unwrap_or_else(|| self.sha256_digest())
    }
}

/// The data returned by an OCI registry after a successful push
/// operation is completed
pub struct PushResponse {
    /// Pullable url for the config
    pub config_url: String,
    /// Pullable url for the manifest
    pub manifest_url: String,
}

/// The data and media type for an image layer
#[derive(Clone)]
pub struct ImageLayer {
    /// The data of this layer
    pub data: Vec<u8>,
    /// The media type of this layer
    pub media_type: String,
}

impl ImageLayer {
    /// Constructs a new ImageLayer struct with provided data and media type
    pub fn new(data: Vec<u8>, media_type: String) -> Self {
        ImageLayer { data, media_type }
    }

    /// Constructs a new ImageLayer struct with provided data and
    /// media type application/vnd.oci.image.layer.v1.tar
    pub fn oci_v1(data: Vec<u8>) -> Self {
        Self::new(data, IMAGE_LAYER_MEDIA_TYPE.to_string())
    }
    /// Constructs a new ImageLayer struct with provided data and
    /// media type application/vnd.oci.image.layer.v1.tar+gzip
    pub fn oci_v1_gzip(data: Vec<u8>) -> Self {
        Self::new(data, IMAGE_LAYER_GZIP_MEDIA_TYPE.to_string())
    }

    /// Helper function to compute the sha256 digest of an image layer
    pub fn sha256_digest(&self) -> String {
        sha256_digest(&self.data)
    }
}

/// The data and media type for a configuration object
#[derive(Clone)]
pub struct Config {
    /// The data of this config object
    pub data: Vec<u8>,
    /// The media type of this object
    pub media_type: String,
}

impl Config {
    /// Constructs a new Config struct with provided data and media type
    pub fn new(data: Vec<u8>, media_type: String) -> Self {
        Config { data, media_type }
    }

    /// Constructs a new Config struct with provided data and
    /// media type application/vnd.oci.image.config.v1+json
    pub fn oci_v1(data: Vec<u8>) -> Self {
        Self::new(data, IMAGE_CONFIG_MEDIA_TYPE.to_string())
    }

    /// Helper function to compute the sha256 digest of this config object
    pub fn sha256_digest(&self) -> String {
        sha256_digest(&self.data)
    }
}

/// The OCI client connects to an OCI registry and fetches OCI images.
///
/// An OCI registry is a container registry that adheres to the OCI Distribution
/// specification. DockerHub is one example, as are ACR and GCR. This client
/// provides a native Rust implementation for pulling OCI images.
///
/// Some OCI registries support completely anonymous access. But most require
/// at least an Oauth2 handshake. Typlically, you will want to create a new
/// client, and then run the `auth()` method, which will attempt to get
/// a read-only bearer token. From there, pulling images can be done with
/// the `pull_*` functions.
///
/// For true anonymous access, you can skip `auth()`. This is not recommended
/// unless you are sure that the remote registry does not require Oauth2.
#[derive(Default)]
pub struct Client {
    config: ClientConfig,
    tokens: TokenCache,
    client: reqwest::Client,
}

/// A source that can provide a `ClientConfig`.
/// If you are using this crate in your own application, you can implement this
/// trait on your configuration type so that it can be passed to `Client::from_source`.
pub trait ClientConfigSource {
    /// Provides a `ClientConfig`.
    fn client_config(&self) -> ClientConfig;
}

impl TryFrom<ClientConfig> for Client {
    type Error = anyhow::Error;

    fn try_from(config: ClientConfig) -> Result<Self, Self::Error> {
        let mut client_builder = reqwest::Client::builder()
            .danger_accept_invalid_certs(config.accept_invalid_certificates);

        client_builder = match () {
            #[cfg(feature = "native-tls")]
            () => client_builder.danger_accept_invalid_hostnames(config.accept_invalid_hostnames),
            #[cfg(not(feature = "native-tls"))]
            () => client_builder,
        };

        for c in &config.extra_root_certificates {
            let cert = match c.encoding {
                CertificateEncoding::Der => reqwest::Certificate::from_der(c.data.as_slice())?,
                CertificateEncoding::Pem => reqwest::Certificate::from_pem(c.data.as_slice())?,
            };
            client_builder = client_builder.add_root_certificate(cert);
        }

        Ok(Self {
            config,
            tokens: TokenCache::new(),
            client: client_builder.build()?,
        })
    }
}

impl Client {
    /// Create a new client with the supplied config
    pub fn new(config: ClientConfig) -> Self {
        Client::try_from(config).unwrap_or_else(|err| {
            warn!("Cannot create OCI client from config: {:?}", err);
            warn!("Creating client with default configuration");
            Self {
                config: ClientConfig::default(),
                tokens: TokenCache::new(),
                client: reqwest::Client::new(),
            }
        })
    }

    /// Create a new client with the supplied config
    pub fn from_source(config_source: &impl ClientConfigSource) -> Self {
        Self::new(config_source.client_config())
    }

    /// Pull an image and return the bytes
    ///
    /// The client will check if it's already been authenticated and if
    /// not will attempt to do.
    pub async fn pull(
        &mut self,
        image: &Reference,
        auth: &RegistryAuth,
        accepted_media_types: Vec<&str>,
    ) -> anyhow::Result<ImageData> {
        debug!("Pulling image: {:?}", image);
        let op = RegistryOperation::Pull;
        if !self.tokens.contains_key(image, op) {
            self.auth(image, auth, op).await?;
        }

        let (manifest, digest, config) = self._pull_manifest_and_config(image).await?;

        self.validate_layers(&manifest, accepted_media_types)
            .await?;

        let layers = manifest.layers.iter().map(|layer| {
            // This avoids moving `self` which is &mut Self
            // into the async block. We only want to capture
            // as &Self
            let this = &self;
            async move {
                let mut out: Vec<u8> = Vec::new();
                debug!("Pulling image layer");
                this.pull_blob(image, &layer.digest, &mut out).await?;
                Ok::<_, anyhow::Error>(ImageLayer::new(out, layer.media_type.clone()))
            }
        });

        let layers = future::try_join_all(layers).await?;

        Ok(ImageData {
            layers,
            manifest: Some(manifest),
            config,
            digest: Some(digest),
        })
    }

    /// Push an image and return the uploaded URL of the image
    ///
    /// The client will check if it's already been authenticated and if
    /// not will attempt to do.
    ///
    /// If a manifest is not provided, the client will attempt to generate
    /// it from the provided image and config data.
    ///
    /// Returns pullable URL for the image
    pub async fn push(
        &mut self,
        image_ref: &Reference,
        layers: Vec<ImageLayer>,
        config: Config,
        auth: &RegistryAuth,
        manifest: Option<OciImageManifest>,
    ) -> anyhow::Result<PushResponse> {
        debug!("Pushing image: {:?}", image_ref);
        let op = RegistryOperation::Push;
        if !self.tokens.contains_key(image_ref, op) {
            self.auth(image_ref, auth, op).await?;
        }

        let manifest: OciImageManifest = match manifest {
            Some(m) => m,
            None => self.generate_manifest(&layers, &config),
        };

        // Upload layers
        for layer in layers {
            let digest = layer.sha256_digest();
            self.push_blob(image_ref, layer.data, &digest).await?;
        }

        let config_url = self
            .push_blob(image_ref, config.data, &manifest.config.digest)
            .await?;
        let manifest_url = self.push_manifest(image_ref, &manifest).await?;

        Ok(PushResponse {
            config_url,
            manifest_url,
        })
    }

    /// Pushes a blob to the registry
    ///
    /// Returns the pullable location of the blob
    async fn push_blob(
        &self,
        image: &Reference,
        blob_data: Vec<u8>,
        blob_digest: &str,
    ) -> anyhow::Result<String> {
        let location = self.begin_push_session(image).await?;
        let (end_location, _) = self.push_chunk(&location, image, blob_data, 0).await?;
        self.end_push_session(&end_location, image, blob_digest)
            .await
    }

    /// Perform an OAuth v2 auth request if necessary.
    ///
    /// This performs authorization and then stores the token internally to be used
    /// on other requests.
    async fn auth(
        &mut self,
        image: &Reference,
        authentication: &RegistryAuth,
        operation: RegistryOperation,
    ) -> anyhow::Result<()> {
        debug!("Authorizing for image: {:?}", image);
        // The version request will tell us where to go.
        let url = format!(
            "{}://{}/v2/",
            self.config.protocol.scheme_for(image.resolve_registry()),
            image.resolve_registry()
        );
        debug!(?url);
        let res = self.client.get(&url).send().await?;
        let dist_hdr = match res.headers().get(reqwest::header::WWW_AUTHENTICATE) {
            Some(h) => h,
            None => return Ok(()),
        };

        let auth = WwwAuthenticate::parse_header(&Raw::from(dist_hdr.as_bytes()))?;
        // If challenge_opt is not set it means that no challenge was present, even though the header
        // was present.
        let challenge_opt = match auth.get::<BearerChallenge>() {
            Some(co) => co,
            None => {
                // Fall back to HTTP Basic Auth
                debug!("Falling back to HTTP Basic Auth");
                if let RegistryAuth::Basic(username, password) = authentication {
                    self.tokens.insert(
                        image,
                        operation,
                        RegistryTokenType::Basic(username.to_string(), password.to_string()),
                    );
                }
                return Ok(());
            }
        };

        // Allow for either push or pull authentication
        let scope = match operation {
            RegistryOperation::Pull => format!("repository:{}:pull", image.repository()),
            RegistryOperation::Push => format!("repository:{}:pull,push", image.repository()),
        };

        let challenge = &challenge_opt[0];
        let realm = challenge.realm.as_ref().unwrap();
        let service = challenge.service.as_ref();
        let mut query = vec![("scope", &scope)];

        if let Some(s) = service {
            query.push(("service", s))
        }

        // TODO: At some point in the future, we should support sending a secret to the
        // server for auth. This particular workflow is for read-only public auth.
        debug!(?realm, ?service, ?scope, "Making authentication call");

        let auth_res = self
            .client
            .get(realm)
            .query(&query)
            .apply_authentication(authentication)
            .send()
            .await?;

        match auth_res.status() {
            reqwest::StatusCode::OK => {
                let text = auth_res.text().await?;
                debug!("Received response from auth request: {}", text);
                let token: RegistryToken = serde_json::from_str(&text)
                    .context("Failed to decode registry token from auth request")?;
                debug!("Succesfully authorized for image '{:?}'", image);
                self.tokens
                    .insert(image, operation, RegistryTokenType::Bearer(token));
                Ok(())
            }
            _ => {
                let reason = auth_res.text().await?;
                debug!("Failed to authenticate for image '{:?}': {}", image, reason);
                Err(anyhow::anyhow!("failed to authenticate: {}", reason))
            }
        }
    }

    /// Fetch a manifest's digest from the remote OCI Distribution service.
    ///
    /// If the connection has already gone through authentication, this will
    /// use the bearer token. Otherwise, this will attempt an anonymous pull.
    ///
    /// Will first attempt to read the `Docker-Content-Digest` header using a
    /// HEAD request. If this header is not present, will make a second GET
    /// request and return the SHA256 of the response body.
    pub async fn fetch_manifest_digest(
        &mut self,
        image: &Reference,
        auth: &RegistryAuth,
    ) -> anyhow::Result<String> {
        let op = RegistryOperation::Pull;
        if !self.tokens.contains_key(image, op) {
            self.auth(image, auth, op).await?;
        }

        let url = self.to_v2_manifest_url(image);
        debug!("HEAD image manifest from {}", url);
        let res = RequestBuilderWrapper::from_client(self, |client| client.head(&url))
            .apply_accept(MIME_TYPES_DISTRIBUTION_MANIFEST)?
            .apply_auth(image, RegistryOperation::Pull)?
            .into_request_builder()
            .send()
            .await?;

        trace!(headers=?res.headers(), "Got Headers");
        if res.headers().get("Docker-Content-Digest").is_none() {
            debug!("GET image manifest from {}", url);
            let res = RequestBuilderWrapper::from_client(self, |client| client.get(&url))
                .apply_accept(MIME_TYPES_DISTRIBUTION_MANIFEST)?
                .apply_auth(image, RegistryOperation::Pull)?
                .into_request_builder()
                .send()
                .await?;
            let status = res.status();
            let headers = res.headers().clone();
            trace!(headers=?res.headers(), "Got Headers");
            let text = res.text().await?;
            // The OCI spec technically does not allow any codes but 200, 500, 401, and 404.
            // Obviously, HTTP servers are going to send other codes. This tries to catch the
            // obvious ones (200, 4XX, 5XX). Anything else is just treated as an error.
            match status {
                reqwest::StatusCode::OK => digest_header_value(headers, Some(&text)),
                reqwest::StatusCode::UNAUTHORIZED => anyhow::bail!("Not Authorized"),
                s if s.is_client_error() => {
                    // According to the OCI spec, we should see an error in the message body.
                    let err = serde_json::from_str::<OciEnvelope>(&text)?;
                    // FIXME: This should not have to wrap the error.
                    Err(anyhow::anyhow!("{} on {}", err.errors[0], url))
                }
                s if s.is_server_error() => Err(anyhow::anyhow!("Server error at {}", url)),
                s => Err(anyhow::anyhow!(
                    "An unexpected error occured: code={}, message='{}'",
                    s,
                    text
                )),
            }
        } else {
            let status = res.status();
            let headers = res.headers().clone();
            let text = res.text().await?;
            // The OCI spec technically does not allow any codes but 200, 500, 401, and 404.
            // Obviously, HTTP servers are going to send other codes. This tries to catch the
            // obvious ones (200, 4XX, 5XX). Anything else is just treated as an error.
            match status {
                reqwest::StatusCode::OK => digest_header_value(headers, None),
                reqwest::StatusCode::UNAUTHORIZED => anyhow::bail!("Not Authorized"),
                s if s.is_client_error() => {
                    // According to the OCI spec, we should see an error in the message body.
                    let err = serde_json::from_str::<OciEnvelope>(&text)?;
                    // FIXME: This should not have to wrap the error.
                    Err(anyhow::anyhow!("{} on {}", err.errors[0], url))
                }
                s if s.is_server_error() => Err(anyhow::anyhow!("Server error at {}", url)),
                s => Err(anyhow::anyhow!(
                    "An unexpected error occured: code={}, message='{}'",
                    s,
                    text
                )),
            }
        }
    }

    async fn validate_layers(
        &self,
        manifest: &OciImageManifest,
        accepted_media_types: Vec<&str>,
    ) -> anyhow::Result<()> {
        if manifest.layers.is_empty() {
            return Err(anyhow::anyhow!("no layers to pull"));
        }

        for layer in &manifest.layers {
            if !accepted_media_types.iter().any(|i| i.eq(&layer.media_type)) {
                return Err(anyhow::anyhow!(
                    "incompatible layer media type: {}",
                    layer.media_type
                ));
            }
        }

        Ok(())
    }

    /// Pull a manifest from the remote OCI Distribution service.
    ///
    /// The client will check if it's already been authenticated and if
    /// not will attempt to do.
    ///
    /// A Tuple is returned containing the [OciImageManifest](crate::manifest::OciImageManifest)
    /// and the manifest content digest hash.
    ///
    /// If a multi-platform Image Index manifest is encountered, a platform-specific
    /// Image manifest will be selected using the client's default platform resolution.
    pub async fn pull_image_manifest(
        &mut self,
        image: &Reference,
        auth: &RegistryAuth,
    ) -> anyhow::Result<(OciImageManifest, String)> {
        let op = RegistryOperation::Pull;
        if !self.tokens.contains_key(image, op) {
            self.auth(image, auth, op).await?;
        }

        self._pull_image_manifest(image).await
    }

    /// Pull a manifest from the remote OCI Distribution service.
    ///
    /// The client will check if it's already been authenticated and if
    /// not will attempt to do.
    ///
    /// A Tuple is returned containing the [Manifest](crate::manifest::Manifest)
    /// and the manifest content digest hash.
    pub async fn pull_manifest(
        &mut self,
        image: &Reference,
        auth: &RegistryAuth,
    ) -> anyhow::Result<(OciManifest, String)> {
        let op = RegistryOperation::Pull;
        if !self.tokens.contains_key(image, op) {
            self.auth(image, auth, op).await?;
        }

        self._pull_manifest(image).await
    }

    /// Pull an image manifest from the remote OCI Distribution service.
    ///
    /// If the connection has already gone through authentication, this will
    /// use the bearer token. Otherwise, this will attempt an anonymous pull.
    ///
    /// If a multi-platform Image Index manifest is encountered, a platform-specific
    /// Image manifest will be selected using the client's default platform resolution.
    async fn _pull_image_manifest(
        &self,
        image: &Reference,
    ) -> anyhow::Result<(OciImageManifest, String)> {
        let (manifest, digest) = self._pull_manifest(image).await?;
        match manifest {
            OciManifest::Image(image_manifest) => Ok((image_manifest, digest)),
            OciManifest::ImageIndex(image_index_manifest) => {
                debug!("Inspecting Image Index Manifest");
                let digest = if let Some(resolver) = &self.config.platform_resolver {
                    resolver(&image_index_manifest.manifests)
                } else {
                    return Err(anyhow::anyhow!(
                        "Received Image Index/Manifest List, but platform_resolver was not defined on the client config. Consider setting platform_resolver"
                    ));
                };

                match digest {
                    Some(digest) => {
                        debug!("Selected manifest entry with digest: {}", digest);
                        let manifest_entry_reference = Reference::with_digest(
                            image.registry().to_string(),
                            image.repository().to_string(),
                            digest.clone(),
                        );
                        self._pull_manifest(&manifest_entry_reference)
                            .await
                            .and_then(|(manifest, _digest)| match manifest {
                                OciManifest::Image(manifest) => Ok((manifest, digest)),
                                OciManifest::ImageIndex(_) => Err(anyhow::anyhow!(
                                    "Expected Image Manifest, received Image Index manifest."
                                )),
                            })
                    }
                    None => Err(anyhow::anyhow!(
                        "No entry found in image index manifest matching client's default platform"
                    )),
                }
            }
        }
    }

    /// Pull a manifest from the remote OCI Distribution service.
    ///
    /// If the connection has already gone through authentication, this will
    /// use the bearer token. Otherwise, this will attempt an anonymous pull.
    async fn _pull_manifest(&self, image: &Reference) -> anyhow::Result<(OciManifest, String)> {
        let url = self.to_v2_manifest_url(image);
        debug!("Pulling image manifest from {}", url);

        let res = RequestBuilderWrapper::from_client(self, |client| client.get(&url))
            .apply_accept(MIME_TYPES_DISTRIBUTION_MANIFEST)?
            .apply_auth(image, RegistryOperation::Pull)?
            .into_request_builder()
            .send()
            .await?;

        // The OCI spec technically does not allow any codes but 200, 500, 401, and 404.
        // Obviously, HTTP servers are going to send other codes. This tries to catch the
        // obvious ones (200, 4XX, 5XX). Anything else is just treated as an error.
        match res.status() {
            reqwest::StatusCode::OK => {
                let headers = res.headers().clone();
                let text = res.text().await?;
                let digest = digest_header_value(headers, Some(&text))?;

                self.validate_image_manifest(&text).await?;

                debug!("Parsing response as Manifest: {}", text);
                let manifest = serde_json::from_str(&text).with_context(|| {
                    format!(
                        "Failed to parse response from pulling manifest for '{:?}' as a Manifest",
                        image
                    )
                })?;
                Ok((manifest, digest))
            }
            s if s.is_client_error() => {
                // According to the OCI spec, we should see an error in the message body.
                let err = res.json::<OciEnvelope>().await?;
                // FIXME: This should not have to wrap the error.
                Err(anyhow::anyhow!("{} on {}", err.errors[0], url))
            }
            s if s.is_server_error() => Err(anyhow::anyhow!("Server error at {}", url)),
            s => Err(anyhow::anyhow!(
                "An unexpected error occurred: code={}, message='{}'",
                s,
                res.text().await?
            )),
        }
    }

    async fn validate_image_manifest(&self, text: &str) -> anyhow::Result<()> {
        debug!("validating manifest: {}", text);
        let versioned: Versioned = serde_json::from_str(text)
            .with_context(|| "Failed to parse manifest as a Versioned object")?;
        if versioned.schema_version != 2 {
            return Err(anyhow::anyhow!(
                "unsupported schema version: {}",
                versioned.schema_version
            ));
        }
        if let Some(media_type) = versioned.media_type {
            if media_type != IMAGE_MANIFEST_MEDIA_TYPE
                && media_type != OCI_IMAGE_MEDIA_TYPE
                && media_type != IMAGE_MANIFEST_LIST_MEDIA_TYPE
            {
                return Err(anyhow::anyhow!("unsupported media type: {}", media_type));
            }
        }

        Ok(())
    }

    /// Pull a manifest and its config from the remote OCI Distribution service.
    ///
    /// The client will check if it's already been authenticated and if
    /// not will attempt to do.
    ///
    /// A Tuple is returned containing the [OciImageManifest](crate::manifest::OciImageManifest),
    /// the manifest content digest hash and the contents of the manifests config layer
    /// as a String.
    pub async fn pull_manifest_and_config(
        &mut self,
        image: &Reference,
        auth: &RegistryAuth,
    ) -> anyhow::Result<(OciImageManifest, String, String)> {
        let op = RegistryOperation::Pull;
        if !self.tokens.contains_key(image, op) {
            self.auth(image, auth, op).await?;
        }

        self._pull_manifest_and_config(image)
            .await
            .and_then(|(manifest, digest, config)| {
                Ok((manifest, digest, String::from_utf8(config.data)?))
            })
    }

    async fn _pull_manifest_and_config(
        &mut self,
        image: &Reference,
    ) -> anyhow::Result<(OciImageManifest, String, Config)> {
        let (manifest, digest) = self._pull_image_manifest(image).await?;

        let mut out: Vec<u8> = Vec::new();
        debug!("Pulling config layer");
        self.pull_blob(image, &manifest.config.digest, &mut out)
            .await?;
        let media_type = manifest.config.media_type.clone();
        Ok((manifest, digest, Config::new(out, media_type)))
    }

    /// Pull a single layer from an OCI registry.
    ///
    /// This pulls the layer for a particular image that is identified by
    /// the given digest. The image reference is used to find the
    /// repository and the registry, but it is not used to verify that
    /// the digest is a layer inside of the image. (The manifest is
    /// used for that.)
    async fn pull_blob<T: AsyncWrite + Unpin>(
        &self,
        image: &Reference,
        digest: &str,
        mut out: T,
    ) -> anyhow::Result<()> {
        let url = self.to_v2_blob_url(image.resolve_registry(), image.repository(), digest);
        let mut stream = RequestBuilderWrapper::from_client(self, |client| client.get(&url))
            .apply_accept(MIME_TYPES_DISTRIBUTION_MANIFEST)?
            .apply_auth(image, RegistryOperation::Pull)?
            .into_request_builder()
            .send()
            .await?
            .bytes_stream();

        while let Some(bytes) = stream.next().await {
            out.write_all(&bytes?).await?;
        }

        Ok(())
    }

    /// Begins a session to push an image to registry
    ///
    /// Returns URL with session UUID
    async fn begin_push_session(&self, image: &Reference) -> anyhow::Result<String> {
        let url = &self.to_v2_blob_upload_url(image);
        let res = RequestBuilderWrapper::from_client(self, |client| client.post(url))
            .apply_auth(image, RegistryOperation::Push)?
            .into_request_builder()
            .header("Content-Length", 0)
            .send()
            .await?;

        // OCI spec requires the status code be 202 Accepted to successfully begin the push process
        self.extract_location_header(image, res, &reqwest::StatusCode::ACCEPTED)
            .await
    }

    /// Closes the push session
    ///
    /// Returns the pullable URL for the image
    async fn end_push_session(
        &self,
        location: &str,
        image: &Reference,
        digest: &str,
    ) -> anyhow::Result<String> {
        let url = Url::parse_with_params(location, &[("digest", digest)])?;
        let res = RequestBuilderWrapper::from_client(self, |client| client.put(url.clone()))
            .apply_auth(image, RegistryOperation::Push)?
            .into_request_builder()
            .header("Content-Length", 0)
            .send()
            .await?;
        self.extract_location_header(image, res, &reqwest::StatusCode::CREATED)
            .await
    }

    /// Pushes a single chunk of a blob to a registry,
    /// as part of a chunked blob upload.
    ///
    /// Returns the URL location for the next layer
    async fn push_chunk(
        &self,
        location: &str,
        image: &Reference,
        layer: Vec<u8>,
        start_byte: usize,
    ) -> anyhow::Result<(String, usize)> {
        trace!("Pushing chunk. size={}, location={}", layer.len(), location);
        if layer.is_empty() {
            return Err(anyhow::anyhow!("cannot push a chunk without data"));
        };
        let end_byte = start_byte + layer.len() - 1;
        let mut headers = HeaderMap::new();
        headers.insert(
            "Content-Range",
            format!("{}-{}", start_byte, end_byte).parse().unwrap(),
        );
        headers.insert(
            "Content-Length",
            format!("{}", layer.len()).parse().unwrap(),
        );
        headers.insert("Content-Type", "application/octet-stream".parse().unwrap());

        let res = RequestBuilderWrapper::from_client(self, |client| client.patch(location))
            .apply_auth(image, RegistryOperation::Push)?
            .into_request_builder()
            .headers(headers)
            .body(layer)
            .send()
            .await?;

        // Returns location for next chunk and the start byte for the next range
        Ok((
            self.extract_location_header(image, res, &reqwest::StatusCode::ACCEPTED)
                .await?,
            end_byte + 1,
        ))
    }

    /// Pushes the manifest for a specified image
    ///
    /// Returns pullable manifest URL
    async fn push_manifest(
        &self,
        image: &Reference,
        manifest: &OciImageManifest,
    ) -> anyhow::Result<String> {
        let url = self.to_v2_manifest_url(image);

        let mut headers = HeaderMap::new();
        headers.insert(
            "Content-Type",
            manifest
                .media_type
                .as_deref()
                .unwrap_or(OCI_IMAGE_MEDIA_TYPE)
                .parse()
                .unwrap(),
        );

        // Serialize the manifest with a canonical json formatter, as described at
        // https://github.com/opencontainers/image-spec/blob/main/considerations.md#json
        let mut body = Vec::new();
        let mut ser = serde_json::Serializer::with_formatter(&mut body, CanonicalFormatter::new());
        manifest.serialize(&mut ser).unwrap();

        let res = RequestBuilderWrapper::from_client(self, |client| client.put(url.clone()))
            .apply_auth(image, RegistryOperation::Push)?
            .into_request_builder()
            .headers(headers)
            .body(body)
            .send()
            .await?;

        self.extract_location_header(image, res, &reqwest::StatusCode::CREATED)
            .await
    }

    async fn extract_location_header(
        &self,
        image: &Reference,
        res: reqwest::Response,
        expected_status: &reqwest::StatusCode,
    ) -> anyhow::Result<String> {
        if res.status().eq(expected_status) {
            let location_header = res.headers().get("Location");
            match location_header {
                None => Err(anyhow::anyhow!("registry did not return a location header")),
                Some(lh) => self.location_header_to_url(image, lh),
            }
        } else {
            Err(anyhow::anyhow!(
                "An unexpected error occured: code={}, message='{}'",
                res.status(),
                res.text().await?
            ))
        }
    }

    /// Helper function to convert location header to URL
    ///
    /// Location may be absolute (containing the protocol and/or hostname), or relative (containing just the URL path)
    /// Returns a properly formatted absolute URL
    fn location_header_to_url(
        &self,
        image: &Reference,
        location_header: &reqwest::header::HeaderValue,
    ) -> anyhow::Result<String> {
        let lh = location_header.to_str().map_err(anyhow::Error::new)?;
        if lh.starts_with("/v2/") {
            Ok(format!(
                "{}://{}{}",
                self.config.protocol.scheme_for(image.resolve_registry()),
                image.resolve_registry(),
                lh
            ))
        } else {
            Ok(lh.to_string())
        }
    }

    fn generate_manifest(&self, layers: &[ImageLayer], config: &Config) -> OciImageManifest {
        let mut manifest = OciImageManifest::default();

        manifest.config.media_type = config.media_type.to_string();
        manifest.config.size = config.data.len() as i64;
        manifest.config.digest = sha256_digest(&config.data);

        for layer in layers {
            let digest = sha256_digest(&layer.data);

            //TODO: Determine necessity of generating an image title
            let mut annotations = HashMap::new();
            annotations.insert(
                "org.opencontainers.image.title".to_string(),
                digest.to_string(),
            );

            let descriptor = OciDescriptor {
                size: layer.data.len() as i64,
                digest,
                media_type: layer.media_type.to_string(),
                annotations: Some(annotations),
                ..Default::default()
            };

            manifest.layers.push(descriptor);
        }

        manifest
    }

    /// Convert a Reference to a v2 manifest URL.
    fn to_v2_manifest_url(&self, reference: &Reference) -> String {
        if let Some(digest) = reference.digest() {
            format!(
                "{}://{}/v2/{}/manifests/{}",
                self.config
                    .protocol
                    .scheme_for(reference.resolve_registry()),
                reference.resolve_registry(),
                reference.repository(),
                digest,
            )
        } else {
            format!(
                "{}://{}/v2/{}/manifests/{}",
                self.config
                    .protocol
                    .scheme_for(reference.resolve_registry()),
                reference.resolve_registry(),
                reference.repository(),
                reference.tag().unwrap_or("latest")
            )
        }
    }

    /// Convert a Reference to a v2 blob (layer) URL.
    fn to_v2_blob_url(&self, registry: &str, repository: &str, digest: &str) -> String {
        format!(
            "{}://{}/v2/{}/blobs/{}",
            self.config.protocol.scheme_for(registry),
            registry,
            repository,
            digest,
        )
    }

    /// Convert a Reference to a v2 blob upload URL.
    fn to_v2_blob_upload_url(&self, reference: &Reference) -> String {
        self.to_v2_blob_url(
            reference.resolve_registry(),
            reference.repository(),
            "uploads/",
        )
    }
}

/// The request builder wrapper allows to be instantiated from a
/// `Client` and allows composable operations on the request builder,
/// to produce a `RequestBuilder` object that can be executed.
struct RequestBuilderWrapper<'a> {
    client: &'a Client,
    request_builder: RequestBuilder,
}

// RequestBuilderWrapper type management
impl<'a> RequestBuilderWrapper<'a> {
    /// Create a `RequestBuilderWrapper` from a `Client` instance, by
    /// instantiating the internal `RequestBuilder` with the provided
    /// function `f`.
    fn from_client(
        client: &'a Client,
        f: impl Fn(&reqwest::Client) -> RequestBuilder,
    ) -> RequestBuilderWrapper {
        let request_builder = f(&client.client);
        RequestBuilderWrapper {
            client,
            request_builder,
        }
    }

    // Produces a final `RequestBuilder` out of this `RequestBuilderWrapper`
    fn into_request_builder(self) -> RequestBuilder {
        self.request_builder
    }
}

// Composable functions applicable to a `RequestBuilderWrapper`
impl<'a> RequestBuilderWrapper<'a> {
    fn apply_accept(&self, accept: &[&str]) -> anyhow::Result<RequestBuilderWrapper> {
        let request_builder = self
            .request_builder
            .try_clone()
            .ok_or_else(|| anyhow!("could not clone request builder"))?
            .header("Accept", Vec::from(accept).join(", "));

        Ok(RequestBuilderWrapper {
            client: self.client,
            request_builder,
        })
    }

    /// Updates request as necessary for authentication.
    ///
    /// If the struct has Some(bearer), this will insert the bearer token in an
    /// Authorization header. It will also set the Accept header, which must
    /// be set on all OCI Registry requests. If the struct has HTTP Basic Auth
    /// credentials, these will be configured.
    fn apply_auth(
        &self,
        image: &Reference,
        op: RegistryOperation,
    ) -> anyhow::Result<RequestBuilderWrapper> {
        let mut headers = HeaderMap::new();

        if let Some(token) = self.client.tokens.get(image, op) {
            match token {
                RegistryTokenType::Bearer(token) => {
                    debug!("Using bearer token authentication.");
                    headers.insert("Authorization", token.bearer_token().parse().unwrap());
                }
                RegistryTokenType::Basic(username, password) => {
                    debug!("Using HTTP basic authentication.");
                    return Ok(RequestBuilderWrapper {
                        client: self.client,
                        request_builder: self
                            .request_builder
                            .try_clone()
                            .ok_or_else(|| anyhow!("could not clone request builder"))?
                            .headers(headers)
                            .basic_auth(username.to_string(), Some(password.to_string())),
                    });
                }
            }
        }
        Ok(RequestBuilderWrapper {
            client: self.client,
            request_builder: self
                .request_builder
                .try_clone()
                .ok_or_else(|| anyhow!("could not clone request builder"))?
                .headers(headers),
        })
    }
}

/// The encoding of the certificate
#[derive(Debug, Clone)]
pub enum CertificateEncoding {
    #[allow(missing_docs)]
    Der,
    #[allow(missing_docs)]
    Pem,
}

/// A x509 certificate
#[derive(Debug, Clone)]
pub struct Certificate {
    /// Which encoding is used by the certificate
    pub encoding: CertificateEncoding,

    /// Actual certificate
    pub data: Vec<u8>,
}

/// A client configuration
pub struct ClientConfig {
    /// Which protocol the client should use
    pub protocol: ClientProtocol,

    /// Accept invalid hostname. Defaults to false
    #[cfg(feature = "native-tls")]
    pub accept_invalid_hostnames: bool,

    /// Accept invalid certificates. Defaults to false
    pub accept_invalid_certificates: bool,

    /// A list of extra root certificate to trust. This can be used to connect
    /// to servers using self-signed certificates
    pub extra_root_certificates: Vec<Certificate>,

    /// A function that defines the client's behaviour if an Image Index Manifest
    /// (i.e Manifest List) is encountered when pulling an image.
    /// Defaults to [current_platform_resolver](self::current_platform_resolver),
    /// which attempts to choose an image matching the running OS and Arch.
    ///
    /// If set to None, an error is raised if an Image Index manifest is received
    /// during an image pull.
    pub platform_resolver: Option<Box<PlatformResolverFn>>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            protocol: ClientProtocol::default(),
            #[cfg(feature = "native-tls")]
            accept_invalid_hostnames: false,
            accept_invalid_certificates: false,
            extra_root_certificates: Vec::new(),
            platform_resolver: Some(Box::new(current_platform_resolver)),
        }
    }
}

// Be explicit about the traits supported by this type. This is needed to use
// the Client behind a dynamic reference.
// Something similar to what is described here: https://users.rust-lang.org/t/how-to-send-function-closure-to-another-thread/43549
type PlatformResolverFn = dyn Fn(&[ImageIndexEntry]) -> Option<String> + Send + Sync;

/// A platform resolver that chooses the first linux/amd64 variant, if present
pub fn linux_amd64_resolver(manifests: &[ImageIndexEntry]) -> Option<String> {
    manifests
        .iter()
        .find(|entry| {
            entry.platform.as_ref().map_or(false, |platform| {
                platform.os == "linux" && platform.architecture == "amd64"
            })
        })
        .map(|entry| entry.digest.clone())
}

const MACOS: &str = "macos";
const DARWIN: &str = "darwin";

fn go_os() -> &'static str {
    // Massage Rust OS var to GO OS:
    // - Rust: https://doc.rust-lang.org/std/env/consts/constant.OS.html
    // - Go: https://golang.org/doc/install/source#environment
    match std::env::consts::OS {
        MACOS => DARWIN,
        other => other,
    }
}

const X86_64: &str = "x86_64";
const AMD64: &str = "amd64";
const X86: &str = "x86";
const AMD: &str = "amd";
const ARM64: &str = "arm64";
const AARCH64: &str = "aarch64";

fn go_arch() -> &'static str {
    // Massage Rust Architecture vars to GO equivalent:
    // - Rust: https://doc.rust-lang.org/std/env/consts/constant.ARCH.html
    // - Go: https://golang.org/doc/install/source#environment
    match std::env::consts::ARCH {
        X86_64 => AMD64,
        X86 => AMD,
        AARCH64 => ARM64,
        other => other,
    }
}

/// A platform resolver that chooses the first variant matching the running OS/Arch, if present.
/// Doesn't currently handle platform.variants.
pub fn current_platform_resolver(manifests: &[ImageIndexEntry]) -> Option<String> {
    manifests
        .iter()
        .find(|entry| {
            entry.platform.as_ref().map_or(false, |platform| {
                platform.os == go_os() && platform.architecture == go_arch()
            })
        })
        .map(|entry| entry.digest.clone())
}

/// The protocol that the client should use to connect
#[derive(Debug, Clone, PartialEq)]
pub enum ClientProtocol {
    #[allow(missing_docs)]
    Http,
    #[allow(missing_docs)]
    Https,
    #[allow(missing_docs)]
    HttpsExcept(Vec<String>),
}

impl Default for ClientProtocol {
    fn default() -> Self {
        ClientProtocol::Https
    }
}

impl ClientProtocol {
    fn scheme_for(&self, registry: &str) -> &str {
        match self {
            ClientProtocol::Https => "https",
            ClientProtocol::Http => "http",
            ClientProtocol::HttpsExcept(exceptions) => {
                if exceptions.contains(&registry.to_owned()) {
                    "http"
                } else {
                    "https"
                }
            }
        }
    }
}

#[derive(Clone)]
struct BearerChallenge {
    pub realm: Option<String>,
    pub service: Option<String>,
    pub scope: Option<String>,
}

impl Challenge for BearerChallenge {
    fn challenge_name() -> &'static str {
        "Bearer"
    }

    fn from_raw(raw: RawChallenge) -> Option<Self> {
        match raw {
            RawChallenge::Token68(_) => None,
            RawChallenge::Fields(mut map) => Some(BearerChallenge {
                realm: map.remove("realm"),
                scope: map.remove("scope"),
                service: map.remove("service"),
            }),
        }
    }

    fn into_raw(self) -> RawChallenge {
        let mut map = ChallengeFields::new();
        if let Some(realm) = self.realm {
            map.insert_static_quoting("realm", realm);
        }
        if let Some(scope) = self.scope {
            map.insert_static_quoting("scope", scope);
        }
        if let Some(service) = self.service {
            map.insert_static_quoting("service", service);
        }
        RawChallenge::Fields(map)
    }
}

/// Extract `Docker-Content-Digest` header from manifest GET or HEAD request.
/// Can optionally supply a response body (i.e. the manifest itself) to
/// fallback to manually hashing this content. This should only be done if the
/// response body contains the image manifest.
fn digest_header_value(headers: HeaderMap, body: Option<&str>) -> anyhow::Result<String> {
    let digest_header = headers.get("Docker-Content-Digest");
    match digest_header {
        None => {
            if let Some(body) = body {
                // Fallback to hashing payload (tested with ECR)
                let digest = sha2::Sha256::digest(body.as_bytes());
                let hex = format!("sha256:{:x}", digest);
                debug!(%hex, "Computed digest of manifest payload.");
                Ok(hex)
            } else {
                Err(anyhow::anyhow!("registry did not return a digest header"))
            }
        }
        Some(hv) => hv
            .to_str()
            .map(|s| s.to_string())
            .map_err(anyhow::Error::new),
    }
}

/// Computes the SHA256 digest of a byte vector
fn sha256_digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", sha2::Sha256::digest(bytes))
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::manifest::{self, IMAGE_DOCKER_LAYER_GZIP_MEDIA_TYPE};
    use std::convert::TryFrom;

    const HELLO_IMAGE_NO_TAG: &str = "webassembly.azurecr.io/hello-wasm";
    const HELLO_IMAGE_TAG: &str = "webassembly.azurecr.io/hello-wasm:v1";
    const HELLO_IMAGE_DIGEST: &str = "webassembly.azurecr.io/hello-wasm@sha256:51d9b231d5129e3ffc267c9d455c49d789bf3167b611a07ab6e4b3304c96b0e7";
    const HELLO_IMAGE_TAG_AND_DIGEST: &str = "webassembly.azurecr.io/hello-wasm:v1@sha256:51d9b231d5129e3ffc267c9d455c49d789bf3167b611a07ab6e4b3304c96b0e7";
    const TEST_IMAGES: &[&str] = &[
        // TODO(jlegrone): this image cannot be pulled currently because no `latest`
        //                 tag exists on the image repository. Re-enable this image
        //                 in tests once `latest` is published.
        // HELLO_IMAGE_NO_TAG,
        HELLO_IMAGE_TAG,
        HELLO_IMAGE_DIGEST,
        HELLO_IMAGE_TAG_AND_DIGEST,
    ];
    const GHCR_IO_IMAGE: &str = "ghcr.io/krustlet/oci-distribution/hello-wasm:v1";
    const DOCKER_IO_IMAGE: &str = "docker.io/library/hello-world@sha256:37a0b92b08d4919615c3ee023f7ddb068d12b8387475d64c622ac30f45c29c51";

    #[test]
    fn test_apply_accept() -> Result<(), anyhow::Error> {
        assert_eq!(
            RequestBuilderWrapper::from_client(&Client::default(), |client| client
                .get("https://example.com/some/module.wasm"))
            .apply_accept(&["*/*"])?
            .into_request_builder()
            .build()?
            .headers()["Accept"],
            "*/*"
        );

        assert_eq!(
            RequestBuilderWrapper::from_client(&Client::default(), |client| client
                .get("https://example.com/some/module.wasm"))
            .apply_accept(MIME_TYPES_DISTRIBUTION_MANIFEST)?
            .into_request_builder()
            .build()?
            .headers()["Accept"],
            MIME_TYPES_DISTRIBUTION_MANIFEST.join(", ")
        );

        Ok(())
    }

    #[test]
    fn test_apply_auth_no_token() -> Result<(), anyhow::Error> {
        assert!(
            !RequestBuilderWrapper::from_client(&Client::default(), |client| client
                .get("https://example.com/some/module.wasm"))
            .apply_auth(
                &Reference::try_from(HELLO_IMAGE_TAG)?,
                RegistryOperation::Pull
            )?
            .into_request_builder()
            .build()?
            .headers()
            .contains_key("Authorization")
        );

        Ok(())
    }

    #[test]
    fn test_apply_auth_bearer_token() -> Result<(), anyhow::Error> {
        use hmac::{Hmac, NewMac};
        use jwt::SignWithKey;
        use sha2::Sha256;
        let mut client = Client::default();
        let header = jwt::header::Header {
            algorithm: jwt::algorithm::AlgorithmType::Hs256,
            key_id: None,
            type_: None,
            content_type: None,
        };
        let claims: jwt::claims::Claims = Default::default();
        let key: Hmac<Sha256> = Hmac::new_from_slice(b"some-secret").unwrap();
        let token = jwt::Token::new(header, claims)
            .sign_with_key(&key)?
            .as_str()
            .to_string();

        client.tokens.insert(
            &Reference::try_from(HELLO_IMAGE_TAG)?,
            RegistryOperation::Pull,
            RegistryTokenType::Bearer(RegistryToken::Token {
                token: token.clone(),
            }),
        );
        assert_eq!(
            RequestBuilderWrapper::from_client(&client, |client| client
                .get("https://example.com/some/module.wasm"))
            .apply_auth(
                &Reference::try_from(HELLO_IMAGE_TAG)?,
                RegistryOperation::Pull
            )?
            .into_request_builder()
            .build()?
            .headers()["Authorization"],
            format!("Bearer {}", &token)
        );

        Ok(())
    }

    #[test]
    fn test_to_v2_blob_url() {
        let image = Reference::try_from(HELLO_IMAGE_TAG).expect("failed to parse reference");
        let blob_url = Client::default().to_v2_blob_url(
            image.registry(),
            image.repository(),
            "sha256:deadbeef",
        );
        assert_eq!(
            blob_url,
            "https://webassembly.azurecr.io/v2/hello-wasm/blobs/sha256:deadbeef"
        )
    }

    #[test]
    fn test_to_v2_manifest() {
        let c = Client::default();

        for &(image, expected_uri) in [
            (HELLO_IMAGE_NO_TAG, "https://webassembly.azurecr.io/v2/hello-wasm/manifests/latest"), // TODO: confirm this is the right translation when no tag
            (HELLO_IMAGE_TAG, "https://webassembly.azurecr.io/v2/hello-wasm/manifests/v1"),
            (HELLO_IMAGE_DIGEST, "https://webassembly.azurecr.io/v2/hello-wasm/manifests/sha256:51d9b231d5129e3ffc267c9d455c49d789bf3167b611a07ab6e4b3304c96b0e7"),
            (HELLO_IMAGE_TAG_AND_DIGEST, "https://webassembly.azurecr.io/v2/hello-wasm/manifests/sha256:51d9b231d5129e3ffc267c9d455c49d789bf3167b611a07ab6e4b3304c96b0e7"),
            ].iter() {
                let reference = Reference::try_from(image).expect("failed to parse reference");
                assert_eq!(c.to_v2_manifest_url(&reference), expected_uri);
            }
    }

    #[test]
    fn test_to_v2_blob_upload_url() {
        let image = Reference::try_from(HELLO_IMAGE_TAG).expect("failed to parse reference");
        let blob_url = Client::default().to_v2_blob_upload_url(&image);

        assert_eq!(
            blob_url,
            "https://webassembly.azurecr.io/v2/hello-wasm/blobs/uploads/"
        )
    }

    #[test]
    fn manifest_url_generation_respects_http_protocol() {
        let c = Client::new(ClientConfig {
            protocol: ClientProtocol::Http,
            ..Default::default()
        });
        let reference = Reference::try_from("webassembly.azurecr.io/hello:v1".to_owned())
            .expect("Could not parse reference");
        assert_eq!(
            "http://webassembly.azurecr.io/v2/hello/manifests/v1",
            c.to_v2_manifest_url(&reference)
        );
    }

    #[test]
    fn blob_url_generation_respects_http_protocol() {
        let c = Client::new(ClientConfig {
            protocol: ClientProtocol::Http,
            ..Default::default()
        });
        let reference = Reference::try_from("webassembly.azurecr.io/hello@sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".to_owned())
            .expect("Could not parse reference");
        assert_eq!(
            "http://webassembly.azurecr.io/v2/hello/blobs/sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
            c.to_v2_blob_url(
                reference.registry(),
                reference.repository(),
                reference.digest().unwrap()
            )
        );
    }

    #[test]
    fn manifest_url_generation_uses_https_if_not_on_exception_list() {
        let insecure_registries = vec!["localhost".to_owned(), "oci.registry.local".to_owned()];
        let protocol = ClientProtocol::HttpsExcept(insecure_registries);
        let c = Client::new(ClientConfig {
            protocol,
            ..Default::default()
        });
        let reference = Reference::try_from("webassembly.azurecr.io/hello:v1".to_owned())
            .expect("Could not parse reference");
        assert_eq!(
            "https://webassembly.azurecr.io/v2/hello/manifests/v1",
            c.to_v2_manifest_url(&reference)
        );
    }

    #[test]
    fn manifest_url_generation_uses_http_if_on_exception_list() {
        let insecure_registries = vec!["localhost".to_owned(), "oci.registry.local".to_owned()];
        let protocol = ClientProtocol::HttpsExcept(insecure_registries);
        let c = Client::new(ClientConfig {
            protocol,
            ..Default::default()
        });
        let reference = Reference::try_from("oci.registry.local/hello:v1".to_owned())
            .expect("Could not parse reference");
        assert_eq!(
            "http://oci.registry.local/v2/hello/manifests/v1",
            c.to_v2_manifest_url(&reference)
        );
    }

    #[test]
    fn blob_url_generation_uses_https_if_not_on_exception_list() {
        let insecure_registries = vec!["localhost".to_owned(), "oci.registry.local".to_owned()];
        let protocol = ClientProtocol::HttpsExcept(insecure_registries);
        let c = Client::new(ClientConfig {
            protocol,
            ..Default::default()
        });
        let reference = Reference::try_from("webassembly.azurecr.io/hello@sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".to_owned())
            .expect("Could not parse reference");
        assert_eq!(
            "https://webassembly.azurecr.io/v2/hello/blobs/sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
            c.to_v2_blob_url(
                reference.registry(),
                reference.repository(),
                reference.digest().unwrap()
            )
        );
    }

    #[test]
    fn blob_url_generation_uses_http_if_on_exception_list() {
        let insecure_registries = vec!["localhost".to_owned(), "oci.registry.local".to_owned()];
        let protocol = ClientProtocol::HttpsExcept(insecure_registries);
        let c = Client::new(ClientConfig {
            protocol,
            ..Default::default()
        });
        let reference = Reference::try_from("oci.registry.local/hello@sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".to_owned())
            .expect("Could not parse reference");
        assert_eq!(
            "http://oci.registry.local/v2/hello/blobs/sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
            c.to_v2_blob_url(
                reference.registry(),
                reference.repository(),
                reference.digest().unwrap()
            )
        );
    }

    #[test]
    fn can_generate_valid_digest() {
        let bytes = b"hellobytes";
        let hash = sha256_digest(&bytes.to_vec());

        let combination = vec![b"hello".to_vec(), b"bytes".to_vec()];
        let combination_hash =
            sha256_digest(&combination.into_iter().flatten().collect::<Vec<u8>>());

        assert_eq!(
            hash,
            "sha256:fdbd95aafcbc814a2600fcc54c1e1706f52d2f9bf45cf53254f25bcd7599ce99"
        );
        assert_eq!(
            combination_hash,
            "sha256:fdbd95aafcbc814a2600fcc54c1e1706f52d2f9bf45cf53254f25bcd7599ce99"
        );
    }

    #[test]
    fn test_registry_token_deserialize() {
        // 'token' field, standalone
        let text = r#"{"token": "abc"}"#;
        let res: Result<RegistryToken, serde_json::Error> = serde_json::from_str(text);
        assert!(res.is_ok());
        let rt = res.unwrap();
        assert_eq!(rt.token(), "abc");

        // 'access_token' field, standalone
        let text = r#"{"access_token": "xyz"}"#;
        let res: Result<RegistryToken, serde_json::Error> = serde_json::from_str(text);
        assert!(res.is_ok());
        let rt = res.unwrap();
        assert_eq!(rt.token(), "xyz");

        // both 'token' and 'access_token' fields, 'token' field takes precedence
        let text = r#"{"access_token": "xyz", "token": "abc"}"#;
        let res: Result<RegistryToken, serde_json::Error> = serde_json::from_str(text);
        assert!(res.is_ok());
        let rt = res.unwrap();
        assert_eq!(rt.token(), "abc");

        // both 'token' and 'access_token' fields, 'token' field takes precedence (reverse order)
        let text = r#"{"token": "abc", "access_token": "xyz"}"#;
        let res: Result<RegistryToken, serde_json::Error> = serde_json::from_str(text);
        assert!(res.is_ok());
        let rt = res.unwrap();
        assert_eq!(rt.token(), "abc");

        // non-string fields do not break parsing
        let text = r#"{"aaa": 300, "access_token": "xyz", "token": "abc", "zzz": 600}"#;
        let res: Result<RegistryToken, serde_json::Error> = serde_json::from_str(text);
        assert!(res.is_ok());

        // Note: tokens should always be strings. The next two tests ensure that if one field
        // is invalid (integer), then parse can still succeed if the other field is a string.
        //
        // numeric 'access_token' field, but string 'token' field does not in parse error
        let text = r#"{"access_token": 300, "token": "abc"}"#;
        let res: Result<RegistryToken, serde_json::Error> = serde_json::from_str(text);
        assert!(res.is_ok());
        let rt = res.unwrap();
        assert_eq!(rt.token(), "abc");

        // numeric 'token' field, but string 'accesss_token' field does not in parse error
        let text = r#"{"access_token": "xyz", "token": 300}"#;
        let res: Result<RegistryToken, serde_json::Error> = serde_json::from_str(text);
        assert!(res.is_ok());
        let rt = res.unwrap();
        assert_eq!(rt.token(), "xyz");

        // numeric 'token' field results in parse error
        let text = r#"{"token": 300}"#;
        let res: Result<RegistryToken, serde_json::Error> = serde_json::from_str(text);
        assert!(res.is_err());

        // numeric 'access_token' field results in parse error
        let text = r#"{"access_token": 300}"#;
        let res: Result<RegistryToken, serde_json::Error> = serde_json::from_str(text);
        assert!(res.is_err());

        // object 'token' field results in parse error
        let text = r#"{"token": {"some": "thing"}}"#;
        let res: Result<RegistryToken, serde_json::Error> = serde_json::from_str(text);
        assert!(res.is_err());

        // object 'access_token' field results in parse error
        let text = r#"{"access_token": {"some": "thing"}}"#;
        let res: Result<RegistryToken, serde_json::Error> = serde_json::from_str(text);
        assert!(res.is_err());

        // missing fields results in parse error
        let text = r#"{"some": "thing"}"#;
        let res: Result<RegistryToken, serde_json::Error> = serde_json::from_str(text);
        assert!(res.is_err());

        // bad JSON results in parse error
        let text = r#"{"token": "abc""#;
        let res: Result<RegistryToken, serde_json::Error> = serde_json::from_str(text);
        assert!(res.is_err());

        // worse JSON results in parse error
        let text = r#"_ _ _ kjbwef??98{9898 }} }}"#;
        let res: Result<RegistryToken, serde_json::Error> = serde_json::from_str(text);
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn test_auth() {
        for &image in TEST_IMAGES {
            let reference = Reference::try_from(image).expect("failed to parse reference");
            let mut c = Client::default();
            c.auth(
                &reference,
                &RegistryAuth::Anonymous,
                RegistryOperation::Pull,
            )
            .await
            .expect("result from auth request");

            let tok = c
                .tokens
                .get(&reference, RegistryOperation::Pull)
                .expect("token is available");
            // We test that the token is longer than a minimal hash.
            if let RegistryTokenType::Bearer(tok) = tok {
                assert!(tok.token().len() > 64);
            } else {
                panic!("Unexpeted Basic Auth Token");
            }
        }
    }

    #[tokio::test]
    async fn test_pull_manifest_private() {
        for &image in TEST_IMAGES {
            let reference = Reference::try_from(image).expect("failed to parse reference");
            // Currently, pull_manifest does not perform Authz, so this will fail.
            let c = Client::default();
            c._pull_image_manifest(&reference)
                .await
                .expect_err("pull manifest should fail");

            // But this should pass
            let mut c = Client::default();
            c.auth(
                &reference,
                &RegistryAuth::Anonymous,
                RegistryOperation::Pull,
            )
            .await
            .expect("authenticated");
            let (manifest, _) = c
                ._pull_image_manifest(&reference)
                .await
                .expect("pull manifest should not fail");

            // The test on the manifest checks all fields. This is just a brief sanity check.
            assert_eq!(manifest.schema_version, 2);
            assert!(!manifest.layers.is_empty());
        }
    }

    #[tokio::test]
    async fn test_pull_manifest_public() {
        for &image in TEST_IMAGES {
            let reference = Reference::try_from(image).expect("failed to parse reference");
            let mut c = Client::default();
            let (manifest, _) = c
                .pull_image_manifest(&reference, &RegistryAuth::Anonymous)
                .await
                .expect("pull manifest should not fail");

            // The test on the manifest checks all fields. This is just a brief sanity check.
            assert_eq!(manifest.schema_version, 2);
            assert!(!manifest.layers.is_empty());
        }
    }

    #[tokio::test]
    async fn pull_manifest_and_config_public() {
        for &image in TEST_IMAGES {
            let reference = Reference::try_from(image).expect("failed to parse reference");
            let mut c = Client::default();
            let (manifest, _, config) = c
                .pull_manifest_and_config(&reference, &RegistryAuth::Anonymous)
                .await
                .expect("pull manifest and config should not fail");

            // The test on the manifest checks all fields. This is just a brief sanity check.
            assert_eq!(manifest.schema_version, 2);
            assert!(!manifest.layers.is_empty());
            assert!(!config.is_empty());
        }
    }

    #[tokio::test]
    async fn test_fetch_digest() {
        let mut c = Client::default();

        for &image in TEST_IMAGES {
            let reference = Reference::try_from(image).expect("failed to parse reference");
            c.fetch_manifest_digest(&reference, &RegistryAuth::Anonymous)
                .await
                .expect("pull manifest should not fail");

            // This should pass
            let reference = Reference::try_from(image).expect("failed to parse reference");
            let mut c = Client::default();
            c.auth(
                &reference,
                &RegistryAuth::Anonymous,
                RegistryOperation::Pull,
            )
            .await
            .expect("authenticated");
            let digest = c
                .fetch_manifest_digest(&reference, &RegistryAuth::Anonymous)
                .await
                .expect("pull manifest should not fail");

            assert_eq!(
                digest,
                "sha256:51d9b231d5129e3ffc267c9d455c49d789bf3167b611a07ab6e4b3304c96b0e7"
            );
        }
    }

    #[tokio::test]
    async fn test_pull_blob() {
        let mut c = Client::default();

        for &image in TEST_IMAGES {
            let reference = Reference::try_from(image).expect("failed to parse reference");
            c.auth(
                &reference,
                &RegistryAuth::Anonymous,
                RegistryOperation::Pull,
            )
            .await
            .expect("authenticated");
            let (manifest, _) = c
                ._pull_image_manifest(&reference)
                .await
                .expect("failed to pull manifest");

            // Pull one specific layer
            let mut file: Vec<u8> = Vec::new();
            let layer0 = &manifest.layers[0];

            // This call likes to flake, so we try it at least 5 times
            let mut last_error = None;
            for i in 1..6 {
                if let Err(e) = c.pull_blob(&reference, &layer0.digest, &mut file).await {
                    println!(
                        "Got error on pull_blob call attempt {}. Will retry in 1s: {:?}",
                        i, e
                    );
                    last_error.replace(e);
                    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                } else {
                    last_error = None;
                    break;
                }
            }

            if let Some(e) = last_error {
                panic!("Unable to pull layer: {:?}", e);
            }

            // The manifest says how many bytes we should expect.
            assert_eq!(file.len(), layer0.size as usize);
        }
    }

    #[tokio::test]
    async fn test_pull() {
        for &image in TEST_IMAGES {
            let reference = Reference::try_from(image).expect("failed to parse reference");

            // This call likes to flake, so we try it at least 5 times
            let mut last_error = None;
            let mut image_data = None;
            for i in 1..6 {
                match Client::default()
                    .pull(
                        &reference,
                        &RegistryAuth::Anonymous,
                        vec![manifest::WASM_LAYER_MEDIA_TYPE],
                    )
                    .await
                {
                    Ok(data) => {
                        image_data = Some(data);
                        last_error = None;
                        break;
                    }
                    Err(e) => {
                        println!(
                            "Got error on pull call attempt {}. Will retry in 1s: {:?}",
                            i, e
                        );
                        last_error.replace(e);
                        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                    }
                }
            }

            if let Some(e) = last_error {
                panic!("Unable to pull layer: {:?}", e);
            }

            assert!(image_data.is_some());
            let image_data = image_data.unwrap();
            assert!(!image_data.layers.is_empty());
            assert!(image_data.digest.is_some());
        }
    }

    /// Attempting to pull an image without any layer validation should fail.
    #[tokio::test]
    async fn test_pull_without_layer_validation() {
        for &image in TEST_IMAGES {
            let reference = Reference::try_from(image).expect("failed to parse reference");
            assert!(Client::default()
                .pull(&reference, &RegistryAuth::Anonymous, vec![],)
                .await
                .is_err());
        }
    }

    /// Attempting to pull an image with the wrong list of layer validations should fail.
    #[tokio::test]
    async fn test_pull_wrong_layer_validation() {
        for &image in TEST_IMAGES {
            let reference = Reference::try_from(image).expect("failed to parse reference");
            assert!(Client::default()
                .pull(&reference, &RegistryAuth::Anonymous, vec!["text/plain"],)
                .await
                .is_err());
        }
    }

    #[tokio::test]
    #[ignore]
    /// Requires local registry resolveable at `oci.registry.local`
    async fn can_push_chunk() {
        let mut c = Client::new(ClientConfig {
            protocol: ClientProtocol::Http,
            ..Default::default()
        });
        let url = "oci.registry.local/hello-wasm:v1";
        let image: Reference = url.parse().unwrap();

        c.auth(&image, &RegistryAuth::Anonymous, RegistryOperation::Push)
            .await
            .expect("result from auth request");

        let location = c
            .begin_push_session(&image)
            .await
            .expect("failed to begin push session");

        let image_data: Vec<Vec<u8>> = vec![b"iamawebassemblymodule".to_vec()];

        let (next_location, next_byte) = c
            .push_chunk(&location, &image, image_data[0].clone(), 0)
            .await
            .expect("failed to push layer");

        // Location should include original URL with at session ID appended
        assert!(next_location.len() >= url.len() + "6987887f-0196-45ee-91a1-2dfad901bea0".len());
        assert_eq!(
            next_byte,
            "iamawebassemblymodule".to_string().into_bytes().len()
        );

        let layer_location = c
            .end_push_session(&next_location, &image, &sha256_digest(&image_data[0]))
            .await
            .expect("failed to end push session");

        assert_eq!(layer_location, "http://oci.registry.local/v2/hello-wasm/blobs/sha256:6165c4ad43c0803798b6f2e49d6348c915d52c999a5f890846cee77ea65d230b");
    }

    #[tokio::test]
    #[ignore]
    /// Requires local registry resolveable at `oci.registry.local`
    async fn can_push_multiple_chunks() {
        let mut c = Client::new(ClientConfig {
            protocol: ClientProtocol::Http,
            ..Default::default()
        });
        let sample_uuid = "6987887f-0196-45ee-91a1-2dfad901bea0";
        let url = "oci.registry.local/hello-wasm:v1";
        let image: Reference = url.parse().unwrap();

        c.auth(&image, &RegistryAuth::Anonymous, RegistryOperation::Push)
            .await
            .expect("result from auth request");

        let image_data: Vec<Vec<u8>> = vec![
            b"iamawebassemblymodule".to_vec(),
            b"anotherwebassemblymodule".to_vec(),
            b"lastlayerwasm".to_vec(),
        ];

        let mut location = c
            .begin_push_session(&image)
            .await
            .expect("failed to begin push session");

        let mut start_byte = 0;

        for layer in image_data.clone() {
            let (next_location, next_byte) = c
                .push_chunk(&location, &image, layer.clone(), start_byte)
                .await
                .expect("failed to push layer");

            // Each next location should be valid and include a UUID
            // Each next byte should be the byte after the pushed layer
            assert!(next_location.len() >= url.len() + sample_uuid.len());
            assert_eq!(next_byte, start_byte + layer.len());

            location = next_location;
            start_byte = next_byte;
        }

        let layer_location = c
            .end_push_session(
                &location,
                &image,
                &sha256_digest(
                    &image_data
                        .clone()
                        .into_iter()
                        .flatten()
                        .collect::<Vec<u8>>(),
                ),
            )
            .await
            .expect("failed to end push session");

        assert_eq!(layer_location, "http://oci.registry.local/v2/hello-wasm/blobs/sha256:5aef3de484a7d350ece6f5483047712be7c9a228998ba16242b3e50b5f16605a");
    }

    #[tokio::test]
    #[ignore]
    /// Requires local registry resolveable at `oci.registry.local`
    async fn test_image_roundtrip() {
        let _ = tracing_subscriber::fmt::try_init();
        let mut c = Client::new(ClientConfig {
            protocol: ClientProtocol::HttpsExcept(vec!["oci.registry.local".to_string()]),
            ..Default::default()
        });

        let image: Reference = HELLO_IMAGE_TAG_AND_DIGEST.parse().unwrap();
        c.auth(&image, &RegistryAuth::Anonymous, RegistryOperation::Pull)
            .await
            .expect("authenticated");

        let (manifest, _digest) = c
            ._pull_image_manifest(&image)
            .await
            .expect("failed to pull manifest");

        let image_data = c
            .pull(
                &image,
                &RegistryAuth::Anonymous,
                vec![manifest::WASM_LAYER_MEDIA_TYPE],
            )
            .await
            .expect("failed to pull image");

        let push_image: Reference = "oci.registry.local/hello-wasm:v1".parse().unwrap();
        c.auth(
            &push_image,
            &RegistryAuth::Anonymous,
            RegistryOperation::Push,
        )
        .await
        .expect("authenticated");

        c.push(
            &push_image,
            image_data.layers.clone(),
            image_data.config.clone(),
            &RegistryAuth::Anonymous,
            Some(manifest.clone()),
        )
        .await
        .expect("failed to push image");

        let pulled_image_data = c
            .pull(
                &push_image,
                &RegistryAuth::Anonymous,
                vec![manifest::WASM_LAYER_MEDIA_TYPE],
            )
            .await
            .expect("failed to pull pushed image");

        let (pulled_manifest, _digest) = c
            ._pull_image_manifest(&push_image)
            .await
            .expect("failed to pull pushed image manifest");

        assert!(image_data.layers.len() == 1);
        assert!(pulled_image_data.layers.len() == 1);
        assert_eq!(
            image_data.layers[0].data.len(),
            pulled_image_data.layers[0].data.len()
        );
        assert_eq!(image_data.layers[0].data, pulled_image_data.layers[0].data);

        assert_eq!(manifest.media_type, pulled_manifest.media_type);
        assert_eq!(manifest.schema_version, pulled_manifest.schema_version);
        assert_eq!(manifest.config.digest, pulled_manifest.config.digest);
    }

    #[tokio::test]
    async fn test_platform_resolution() {
        // test that we get an error when we pull a manifest list
        let reference = Reference::try_from(DOCKER_IO_IMAGE).expect("failed to parse reference");
        let mut c = Client::new(ClientConfig {
            platform_resolver: None,
            ..Default::default()
        });
        let err = c
            .pull_image_manifest(&reference, &RegistryAuth::Anonymous)
            .await
            .unwrap_err();
        assert_eq!(
            format!("{}", err),
            "Received Image Index/Manifest List, but platform_resolver was not defined on the client config. Consider setting platform_resolver"
        );

        c = Client::new(ClientConfig {
            platform_resolver: Some(Box::new(linux_amd64_resolver)),
            ..Default::default()
        });
        let (_manifest, digest) = c
            .pull_image_manifest(&reference, &RegistryAuth::Anonymous)
            .await
            .expect("Couldn't pull manifest");
        assert_eq!(
            digest,
            "sha256:f54a58bc1aac5ea1a25d796ae155dc228b3f0e11d046ae276b39c4bf2f13d8c4"
        );
    }

    #[tokio::test]
    async fn test_pull_ghcr_io() {
        let reference = Reference::try_from(GHCR_IO_IMAGE).expect("failed to parse reference");
        let mut c = Client::default();
        let (manifest, _manifest_str) = c
            .pull_image_manifest(&reference, &RegistryAuth::Anonymous)
            .await
            .unwrap();
        assert_eq!(manifest.config.media_type, manifest::WASM_CONFIG_MEDIA_TYPE);
    }

    #[tokio::test]
    #[ignore]
    async fn test_roundtrip_multiple_layers() {
        let _ = tracing_subscriber::fmt::try_init();
        let mut c = Client::new(ClientConfig {
            protocol: ClientProtocol::HttpsExcept(vec!["oci.registry.local".to_string()]),
            ..Default::default()
        });
        let src_image = Reference::try_from("registry:2.7.1").expect("failed to parse reference");
        let dest_image = Reference::try_from("oci.registry.local/registry:roundtrip-test")
            .expect("failed to parse reference");

        let image = c
            .pull(
                &src_image,
                &RegistryAuth::Anonymous,
                vec![IMAGE_DOCKER_LAYER_GZIP_MEDIA_TYPE],
            )
            .await
            .expect("Failed to pull manifest");
        assert!(image.layers.len() > 1);

        let ImageData {
            layers,
            config,
            manifest,
            ..
        } = image;
        c.push(
            &dest_image,
            layers,
            config,
            &RegistryAuth::Anonymous,
            manifest,
        )
        .await
        .expect("Failed to pull manifest");

        c.pull_image_manifest(&dest_image, &RegistryAuth::Anonymous)
            .await
            .expect("Failed to pull manifest");
    }
}
