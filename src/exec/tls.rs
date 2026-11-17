//! Select TLS cryptography explicitly when dependencies enable multiple backends.
pub fn initialize() {
    // Preserve an embedding application's provider and tolerate concurrent startup.
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
}
