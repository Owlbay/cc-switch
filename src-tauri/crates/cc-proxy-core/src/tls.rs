//! 出站 TLS 配置（设计文档 §5.6、§9.2）。
//!
//! 根证书默认合并 webpki-roots 与系统证书，可追加 `extra_ca_file`。CryptoProvider 通过
//! `builder_with_provider` 显式传入 ring，不依赖进程级 `install_default()`，因此嵌入桌面
//! app 时也不会和 app 自己安装的 provider 冲突。禁止在 core 中使用 `ClientConfig::builder()`
//! 系列 API（workspace 构建中 rustls 可能同时启用 ring 与 aws-lc-rs，会导致二义）。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::CertificateDer;
use rustls::{ClientConfig, RootCertStore};

use crate::config::TlsConfig;

#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("读取 extra_ca_file {path} 失败: {message}")]
    ExtraCa { path: PathBuf, message: String },
    #[error("extra_ca_file {path} 中没有可用的证书")]
    ExtraCaEmpty { path: PathBuf },
    #[error("没有可用的根证书")]
    NoRoots,
    #[error("构造 TLS 配置失败: {0}")]
    Rustls(#[from] rustls::Error),
}

/// 构造出站 TLS 的 `ClientConfig`。
pub fn client_config(tls: &TlsConfig) -> Result<Arc<ClientConfig>, TlsError> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    if tls.native_roots {
        let native = rustls_native_certs::load_native_certs();
        if native.certs.is_empty() && !native.errors.is_empty() {
            tracing::warn!(
                errors = native.errors.len(),
                "加载系统根证书失败，仅使用内置根证书"
            );
        }
        // 单条证书无法解析时忽略，不影响其它证书
        let _ = roots.add_parsable_certificates(native.certs);
    }

    if let Some(path) = tls.extra_ca_file.as_deref() {
        let added = add_pem_file(&mut roots, path)?;
        if added == 0 {
            return Err(TlsError::ExtraCaEmpty {
                path: path.to_path_buf(),
            });
        }
    }

    if roots.is_empty() {
        return Err(TlsError::NoRoots);
    }

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

fn add_pem_file(roots: &mut RootCertStore, path: &Path) -> Result<usize, TlsError> {
    let to_error = |message: String| TlsError::ExtraCa {
        path: path.to_path_buf(),
        message,
    };
    let certs = CertificateDer::pem_file_iter(path)
        .map_err(|e| to_error(e.to_string()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| to_error(e.to_string()))?;
    let (added, _ignored) = roots.add_parsable_certificates(certs);
    Ok(added)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_with_builtin_roots_only() {
        let config = client_config(&TlsConfig {
            native_roots: false,
            extra_ca_file: None,
        })
        .unwrap();
        assert!(config.alpn_protocols.is_empty());
    }

    #[test]
    fn builds_with_native_roots() {
        client_config(&TlsConfig::default()).unwrap();
    }

    #[test]
    fn missing_extra_ca_file_is_an_error() {
        let err = client_config(&TlsConfig {
            native_roots: false,
            extra_ca_file: Some(PathBuf::from("/nonexistent/ca.pem")),
        })
        .unwrap_err();
        assert!(matches!(err, TlsError::ExtraCa { .. }), "{err}");
    }

    #[test]
    fn extra_ca_file_without_certs_is_an_error() {
        let dir = std::env::temp_dir().join(format!("cc-proxy-tls-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("empty.pem");
        std::fs::write(&path, "not a certificate\n").unwrap();
        let err = client_config(&TlsConfig {
            native_roots: false,
            extra_ca_file: Some(path.clone()),
        })
        .unwrap_err();
        assert!(matches!(err, TlsError::ExtraCaEmpty { .. }), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
