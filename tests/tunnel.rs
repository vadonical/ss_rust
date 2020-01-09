use env_logger;
use tokio::{
    self,
    net::{TcpStream, UdpSocket},
    prelude::*,
    runtime::Builder,
    time::{self, Duration},
};

use shadowsocks::{
    config::{Config, ConfigType},
    relay::socks5::Address,
    run_local,
    run_server,
};

#[test]
fn tcp_tunnel() {
    let _ = env_logger::try_init();

    let mut local_config = Config::load_from_str(
        r#"{
            "local_port": 9110,
            "local_address": "127.0.0.1",
            "server": "127.0.0.1",
            "server_port": 9120,
            "password": "password",
            "method": "aes-256-gcm"
        }"#,
        ConfigType::Local,
    )
    .unwrap();

    local_config.forward = Some("www.example.com:80".parse::<Address>().unwrap());

    let server_config = Config::load_from_str(
        r#"{
            "server": "127.0.0.1",
            "server_port": 9120,
            "password": "password",
            "method": "aes-256-gcm"
        }"#,
        ConfigType::Server,
    )
    .unwrap();

    let mut rt = Builder::new().basic_scheduler().enable_all().build().unwrap();
    let rt_handle = rt.handle().clone();

    rt.block_on(async move {
        tokio::spawn(run_local(local_config, rt_handle.clone()));
        tokio::spawn(run_server(server_config, rt_handle));

        time::delay_for(Duration::from_secs(1)).await;

        // Connect it directly, because it is now established a TCP tunnel with www.example.com
        let mut stream = TcpStream::connect("127.0.0.1:9110").await.unwrap();

        let req = b"GET / HTTP/1.0\r\nHost: www.example.com\r\nAccept: */*\r\n\r\n";
        stream.write_all(req).await.unwrap();
        stream.flush().await.unwrap();

        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();

        println!("Got reply from server: {}", String::from_utf8(buf).unwrap());
    });
}

#[test]
fn udp_tunnel() {
    let _ = env_logger::try_init();

    let mut local_config = Config::load_from_str(
        r#"{
            "local_port": 9210,
            "local_address": "127.0.0.1",
            "server": "127.0.0.1",
            "server_port": 9220,
            "password": "password",
            "method": "aes-256-gcm",
            "mode": "tcp_and_udp"
        }"#,
        ConfigType::Local,
    )
    .unwrap();

    local_config.forward = Some("127.0.0.1:9230".parse::<Address>().unwrap());

    let server_config = Config::load_from_str(
        r#"{
            "server": "127.0.0.1",
            "server_port": 9220,
            "password": "password",
            "method": "aes-256-gcm",
            "mode": "udp_only"
        }"#,
        ConfigType::Server,
    )
    .unwrap();

    let mut rt = Builder::new().basic_scheduler().enable_all().build().unwrap();
    let rt_handle = rt.handle().clone();

    rt.block_on(async move {
        tokio::spawn(run_local(local_config, rt_handle.clone()));
        tokio::spawn(run_server(server_config, rt_handle));

        // Start a UDP echo server
        tokio::spawn(async {
            let mut socket = UdpSocket::bind("127.0.0.1:9230").await.unwrap();

            let mut buf = vec![0u8; 65536];
            let (n, src) = socket.recv_from(&mut buf).await.unwrap();

            socket.send_to(&buf[..n], src).await.unwrap();
        });

        time::delay_for(Duration::from_secs(1)).await;

        let mut socket = UdpSocket::bind("0.0.0.0:0").await.unwrap();
        socket.send_to(b"HELLO WORLD", "127.0.0.1:9210").await.unwrap();

        let mut buf = vec![0u8; 65536];
        let n = socket.recv(&mut buf).await.unwrap();

        println!("Got reply from server: {}", ::std::str::from_utf8(&buf[..n]).unwrap());
    });
}
