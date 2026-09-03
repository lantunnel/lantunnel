use std::io;
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::time::Duration;

fn parse_addr(raw: &str) -> io::Result<SocketAddr> {
    raw.parse()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid socket address"))
}

fn parse_index(raw: &str) -> io::Result<NonZeroU32> {
    raw.parse::<u32>()
        .ok()
        .and_then(NonZeroU32::new)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid interface index"))
}

fn main() -> io::Result<()> {
    let args = std::env::args().collect::<Vec<_>>();
    if args.len() != 4 || (args[1] != "server" && args[1] != "client") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected server|client, address, interface index",
        ));
    }

    let address = parse_addr(&args[2])?;
    let interface_index = parse_index(&args[3])?;
    let bind = if args[1] == "server" {
        address
    } else if address.is_ipv4() {
        "0.0.0.0:0".parse().unwrap()
    } else {
        "[::]:0".parse().unwrap()
    };
    let socket = tp_transport::quic::bind_tuned_udp_on_interface(bind, Some(interface_index))?;
    socket.set_nonblocking(false)?;
    socket.set_read_timeout(Some(Duration::from_secs(4)))?;
    socket.set_write_timeout(Some(Duration::from_secs(4)))?;

    if args[1] == "server" {
        let mut buffer = [0u8; 16];
        let (length, peer) = socket.recv_from(&mut buffer)?;
        if &buffer[..length] != b"ping" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unexpected probe",
            ));
        }
        socket.send_to(b"pong", peer)?;
        println!("server_ok=true");
    } else {
        socket.send_to(b"ping", address)?;
        let mut buffer = [0u8; 16];
        let (length, peer) = socket.recv_from(&mut buffer)?;
        if peer != address || &buffer[..length] != b"pong" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unexpected response",
            ));
        }
        println!("client_ok=true");
    }
    Ok(())
}
