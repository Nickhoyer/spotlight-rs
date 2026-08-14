//! Launch Sims: opens the Sims 4 Modding Tool, waits for its local server to
//! come up on port 3216, then opens the EA app.

use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::Path;
use std::time::{Duration, Instant};

const MODDING_TOOL: &str = "/Applications/The Sims 4 Modding Tool.app";
const EA_APP: &str = "/Applications/EA app.app";
const PORT: u16 = 3216;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);

pub fn run() -> anyhow::Result<()> {
    spotlight_platform_macos::apps::launch(Path::new(MODDING_TOOL))?;
    wait_for_port(PORT, STARTUP_TIMEOUT)?;
    spotlight_platform_macos::apps::launch(Path::new(EA_APP))?;
    Ok(())
}

fn wait_for_port(port: u16, timeout: Duration) -> anyhow::Result<()> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(250)).is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    anyhow::bail!("nothing listening on port {port} after {timeout:?}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn wait_for_port_sees_listener() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        wait_for_port(port, Duration::from_secs(2)).unwrap();
    }

    #[test]
    fn wait_for_port_times_out_when_closed() {
        // Bind-then-drop yields a port that was just free.
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        assert!(wait_for_port(port, Duration::from_millis(100)).is_err());
    }
}
