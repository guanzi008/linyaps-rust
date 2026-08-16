use std::collections::HashMap;

use zvariant::Value;

const SERVICE: &str = "org.freedesktop.PolicyKit1";
const PATH: &str = "/org/freedesktop/PolicyKit1/Authority";
const INTERFACE: &str = "org.freedesktop.PolicyKit1.Authority";

pub async fn authorize(action: &str, sender: &str) -> Result<(), String> {
    let connection = zbus::Connection::system()
        .await
        .map_err(|error| format!("failed to connect to system bus for polkit: {error}"))?;
    let proxy = zbus::Proxy::new(&connection, SERVICE, PATH, INTERFACE)
        .await
        .map_err(|error| format!("failed to create polkit proxy: {error}"))?;
    let subject = (
        "system-bus-name",
        HashMap::from([("name", Value::from(sender))]),
    );
    let details = HashMap::<&str, &str>::new();
    let (authorized, _challenge, _details): (bool, bool, HashMap<String, String>) = proxy
        .call("CheckAuthorization", &(subject, action, details, 1_u32, ""))
        .await
        .map_err(|error| format!("polkit check failed: {error}"))?;
    if authorized {
        Ok(())
    } else {
        Err("not authorized".to_string())
    }
}
