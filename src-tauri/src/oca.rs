use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

const OCA_PORT: u16 = 50014;
const HTTP_PORT: u16 = 80;

pub async fn ping(ip: &str) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let addr = format!("{}:{}", ip, OCA_PORT);
    match TcpStream::connect_timeout(&addr.parse()?, Duration::from_millis(1500)) {
        Ok(_) => Ok(true),
        Err(_) => Ok(false),
    }
}

pub async fn send_command(ip: &str, command: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let addr = format!("{}:{}", ip, OCA_PORT);
    
    // GetMute commands — lese aktuellen Status
    if command.starts_with("get_mute_") {
        let ch = command.chars().last().unwrap_or('A');
        let get_bytes = match ch {
            'A' => hex_to_bytes("3b00010000001701000100000012000006c41000820300020001"),
            'B' => hex_to_bytes("3b00010000001701000100000012000006c51001020300020001"),
            'C' => hex_to_bytes("3b00010000001701000100000012000006c61001820300020001"),
            'D' => hex_to_bytes("3b00010000001701000100000012000006c71002020300020001"),
            _   => return Ok("muted:unknown".to_string()),
        };
        if let Ok(bytes) = get_bytes {
            if let Ok(mut stream) = TcpStream::connect_timeout(&addr.parse()?, Duration::from_millis(1000)) {
                stream.set_read_timeout(Some(Duration::from_millis(500)))?;
                let _ = stream.write_all(&bytes);
                let mut buf = [0u8; 64];
                if let Ok(n) = stream.read(&mut buf) {
                    if n > 0 {
                        // OCA Response: letztes Byte ist Mute-Status (0=unmuted, 1=muted)
                        let muted = buf[n-1] == 1;
                        return Ok(format!("muted:{}", muted));
                    }
                }
            }
        }
        return Err("No mute status response".into());
    }

    let mut stream = TcpStream::connect_timeout(&addr.parse()?, Duration::from_millis(2000))?;
    stream.set_read_timeout(Some(Duration::from_millis(1000)))?;

    let bytes = match command {
        "mute_A"   => hex_to_bytes("3b00010000001b01000100000012000006c410008205000400020101"),
        "unmute_A" => hex_to_bytes("3b00010000001b01000100000012000006c810008205000400020102"),
        "mute_B"   => hex_to_bytes("3b00010000001b01000100000012000006c510010205000400020101"),
        "unmute_B" => hex_to_bytes("3b00010000001b01000100000012000006c910010205000400020102"),
        "mute_C"   => hex_to_bytes("3b00010000001b01000100000012000006c610018205000400020101"),
        "unmute_C" => hex_to_bytes("3b00010000001b01000100000012000006ca10018205000400020102"),
        "mute_D"   => hex_to_bytes("3b00010000001b01000100000012000006c710020205000400020101"),
        "unmute_D" => hex_to_bytes("3b00010000001b01000100000012000006cb10020205000400020102"),
        "mute_all" => {
            for cmd in &["mute_A","mute_B","mute_C","mute_D"] {
                if let Ok(b) = get_cmd_bytes(cmd) {
                    let _ = stream.write_all(&b);
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
            return Ok("mute_all sent".to_string());
        }
        "unmute_all" => {
            for cmd in &["unmute_A","unmute_B","unmute_C","unmute_D"] {
                if let Ok(b) = get_cmd_bytes(cmd) {
                    let _ = stream.write_all(&b);
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
            return Ok("unmute_all sent".to_string());
        }
        _ => return Err(format!("Unknown command: {}", command).into()),
    }?;

    stream.write_all(&bytes)?;
    let mut buf = [0u8; 64];
    let _ = stream.read(&mut buf);
    Ok(format!("{} sent to {}", command, ip))
}

pub async fn get_status(ip: &str) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
    let targets = [
        (360, 80, "A", 440, 80),
        (360, 120, "B", 440, 120),
        (360, 200, "C", 440, 200),
        (360, 240, "D", 440, 240),
    ];

    let mut channels = Vec::new();
    for (gain_x, gain_y, channel, state_x, state_y) in targets {
        let gain_info = eleinfo(ip, gain_x, gain_y)?;
        let state_info = eleinfo(ip, state_x, state_y)?;

        let gain_value = gain_info
            .get("value")
            .cloned()
            .unwrap_or_else(|| "-- dB".to_string());
        let muted = state_info
            .get("value")
            .and_then(|v| v.parse::<u8>().ok())
            .map(|v| v != 0)
            .unwrap_or(false);

        channels.push(serde_json::json!({
            "channel": channel,
            "gain": gain_value,
            "muted": muted,
            "status": state_info.get("value").cloned().unwrap_or_else(|| "0".to_string()),
        }));
    }

    Ok(serde_json::json!({"ip": ip, "channels": channels}))
}

pub async fn set_gain(ip: &str, channel: usize, current: f32, target: f32) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let coords = match channel {
        0 => (360, 80),
        1 => (360, 120),
        2 => (360, 200),
        3 => (360, 240),
        _ => return Err(format!("Unsupported channel: {}", channel).into()),
    };

    let steps = ((target - current) / 0.5).round() as i32;
    if steps == 0 {
        return Ok(format!("Gain already at {:.1} dB", current));
    }

    let touch_cmd = format!("TOUCH {} {}", coords.0, coords.1);
    let value_cmd = format!("VALUE {:+}", steps);

    let touch_path = format!("/dxcmd.cgi?cmd={}", url_encode(&touch_cmd));
    http_get(ip, &touch_path)?;
    std::thread::sleep(Duration::from_millis(120));

    let value_path = format!("/dxcmd.cgi?cmd={}", url_encode(&value_cmd));
    http_get(ip, &value_path)?;
    Ok(format!("Set gain channel {} by {} steps", channel, steps))
}

fn url_encode(value: &str) -> String {
    value
        .as_bytes()
        .iter()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (*b as char).to_string(),
            _ => format!("%{:02X}", b),
        })
        .collect()
}

fn http_get(ip: &str, path: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let addr = format!("{}:{}", ip, HTTP_PORT);
    let mut stream = TcpStream::connect_timeout(&addr.parse()?, Duration::from_millis(1500))?;
    stream.set_read_timeout(Some(Duration::from_millis(1500)))?;

    let request = format!(
        "GET {} HTTP/1.0\r\nHost: {}\r\nConnection: close\r\n\r\n",
        path, ip
    );
    stream.write_all(request.as_bytes())?;

    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    let response = String::from_utf8_lossy(&response).into_owned();

    if let Some(pos) = response.find("\r\n\r\n") {
        Ok(response[pos + 4..].to_string())
    } else {
        Ok(response)
    }
}

fn eleinfo(ip: &str, x: u32, y: u32) -> Result<HashMap<String, String>, Box<dyn std::error::Error + Send + Sync>> {
    let cmd = format!("ELEINFO {} {}", x, y);
    let path = format!("/dxcmd.cgi?cmd={}", url_encode(&cmd));
    let body = http_get(ip, &path)?;
    if body.starts_with("Error") {
        return Err(body.into());
    }
    parse_eleinfo(&body)
}

fn parse_eleinfo(value: &str) -> Result<HashMap<String, String>, Box<dyn std::error::Error + Send + Sync>> {
    let mut map = HashMap::new();
    for token in value.split(',') {
        if let Some(pos) = token.find('=') {
            let key = token[..pos].trim().to_lowercase();
            let val = token[pos + 1..].trim().to_string();
            map.insert(key, val);
        }
    }
    if map.is_empty() {
        return Err(format!("Invalid ELEINFO response: {}", value).into());
    }
    Ok(map)
}

fn get_cmd_bytes(cmd: &str) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    match cmd {
        "mute_A"   => hex_to_bytes("3b00010000001b01000100000012000006c410008205000400020101"),
        "unmute_A" => hex_to_bytes("3b00010000001b01000100000012000006c810008205000400020102"),
        "mute_B"   => hex_to_bytes("3b00010000001b01000100000012000006c510010205000400020101"),
        "unmute_B" => hex_to_bytes("3b00010000001b01000100000012000006c910010205000400020102"),
        "mute_C"   => hex_to_bytes("3b00010000001b01000100000012000006c610018205000400020101"),
        "unmute_C" => hex_to_bytes("3b00010000001b01000100000012000006ca10018205000400020102"),
        "mute_D"   => hex_to_bytes("3b00010000001b01000100000012000006c710020205000400020101"),
        "unmute_D" => hex_to_bytes("3b00010000001b01000100000012000006cb10020205000400020102"),
        _ => Err(format!("Unknown: {}", cmd).into()),
    }
}

fn hex_to_bytes(hex: &str) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i+2], 16)
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>))
        .collect()
}
