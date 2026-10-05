//! Explain listener failures without changing the user's configured address.
use std::{io, net::SocketAddr};

use tokio::net::TcpListener;

fn bind_error(address: SocketAddr, error: io::Error) -> anyhow::Error {
    let message = match error.kind() {
        io::ErrorKind::AddrNotAvailable => format!(
            "gateway listen address {address} is not assigned to this machine; the IP may have changed or its network interface may not be ready. Update `listen` in the gateway config.json to the current local address, then restart the gateway. No address was changed automatically"
        ),
        io::ErrorKind::AddrInUse => format!(
            "gateway listen address {address} is already in use; stop the conflicting listener or choose another port in the gateway config.json"
        ),
        _ => format!("could not bind gateway to {address}"),
    };
    anyhow::Error::new(error).context(message)
}

pub(crate) async fn bind(address: SocketAddr) -> anyhow::Result<TcpListener> {
    TcpListener::bind(address)
        .await
        .map_err(|error| bind_error(address, error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_address_explains_the_manual_fix_and_preserves_the_cause() {
        let error = bind_error(
            "192.0.2.1:23847".parse().unwrap(),
            io::Error::from(io::ErrorKind::AddrNotAvailable),
        );
        let message = error.to_string();
        assert!(message.contains("IP may have changed"));
        assert!(message.contains("Update `listen`"));
        assert!(message.contains("restart the gateway"));
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::AddrNotAvailable
        );
    }

    #[tokio::test]
    async fn binds_the_requested_interface_and_reports_a_busy_port() {
        let listener = bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let address = listener.local_addr().unwrap();
        assert!(address.ip().is_loopback());
        let error = bind(address).await.unwrap_err();
        assert!(error.to_string().contains("already in use"));
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::AddrInUse
        );
    }
}
