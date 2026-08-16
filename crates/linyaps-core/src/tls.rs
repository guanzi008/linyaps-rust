use std::sync::Once;

static INSTALL_PROVIDER: Once = Once::new();

pub fn install_default_provider() {
    INSTALL_PROVIDER.call_once(|| {
        let _ = rustls_rustcrypto::provider().install_default();
    });
}
