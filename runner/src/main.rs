//! labpad-runner: локальный запуск C-кода для редактора отчёта.
//!
//! Один эндпоинт: POST /run  {"code": "...", "stdin": "..."} -> JSON
//! {ok, stdout, stderr, code, ms, compiler}. Слушает только 127.0.0.1.
//! Компилятор — cc/gcc/clang из PATH, таймаут — coreutils `timeout`.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const MAX_BODY: usize = 512 * 1024;
const MAX_OUT: usize = 64 * 1024;
const RUN_TIMEOUT: u32 = 3;

fn main() {
    let port: u16 = std::env::args()
        .nth(1)
        .and_then(|p| p.trim_start_matches(':').parse().ok())
        .unwrap_or(8788);
    let addr = format!("127.0.0.1:{port}");
    let listener = match TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("labpad-runner: не удалось занять {addr}: {e}");
            std::process::exit(2);
        }
    };
    println!(
        "labpad-runner: http://{addr}  (компилятор: {})",
        compiler_name()
    );
    for stream in listener.incoming().flatten() {
        std::thread::spawn(move || {
            let _ = handle(stream);
        });
    }
}

fn compiler_name() -> String {
    for c in ["cc", "gcc", "clang"] {
        if Command::new(c)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            return c.to_string();
        }
    }
    "нет компилятора".into()
}

fn handle(mut s: TcpStream) -> std::io::Result<()> {
    s.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut head = Vec::new();
    let mut buf = [0u8; 4096];
    let head_end = loop {
        let n = s.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        head.extend_from_slice(&buf[..n]);
        if let Some(i) = find(&head, b"\r\n\r\n") {
            break i + 4;
        }
        if head.len() > 64 * 1024 {
            return reply(&mut s, 431, "слишком большие заголовки");
        }
    };
    let text = String::from_utf8_lossy(&head[..head_end]).to_string();
    let request_line = text.lines().next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");

    if method == "OPTIONS" {
        return reply(&mut s, 204, "");
    }
    if path == "/health" {
        return reply(&mut s, 200, &format!("{{\"ok\":true,\"compiler\":\"{}\"}}", compiler_name()));
    }
    if method != "POST" || path != "/run" {
        return reply(&mut s, 404, "{\"ok\":false,\"error\":\"нет такого пути\"}");
    }

    let len = text
        .lines()
        .find_map(|l| l.strip_prefix("Content-Length:").or_else(|| l.strip_prefix("content-length:")))
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    if len > MAX_BODY {
        return reply(&mut s, 413, "{\"ok\":false,\"error\":\"слишком большой запрос\"}");
    }
    let mut body = head[head_end..].to_vec();
    while body.len() < len {
        let n = s.read(&mut buf)?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&buf[..n]);
    }
    body.truncate(len);
    let body = String::from_utf8_lossy(&body);

    let code = json_str(&body, "code").unwrap_or_default();
    let stdin = json_str(&body, "stdin").unwrap_or_default();
    if code.trim().is_empty() {
        return reply(&mut s, 200, "{\"ok\":false,\"error\":\"пустой код\"}");
    }
    let out = run_c(&code, &stdin);
    reply(&mut s, 200, &out)
}

fn run_c(code: &str, stdin: &str) -> String {
    let cc = compiler_name();
    if cc == "нет компилятора" {
        return "{\"ok\":false,\"error\":\"не найден cc/gcc/clang\"}".into();
    }
    let dir = std::env::temp_dir().join(format!("labpad-run-{}-{}", std::process::id(), nanos()));
    if std::fs::create_dir_all(&dir).is_err() {
        return "{\"ok\":false,\"error\":\"не создать временную папку\"}".into();
    }
    let src = dir.join("main.c");
    let exe = dir.join("prog");
    if std::fs::write(&src, code).is_err() {
        return "{\"ok\":false,\"error\":\"не записать код\"}".into();
    }
    let t0 = Instant::now();
    let build = Command::new(&cc)
        .args(["-O0", "-w", "-o"])
        .arg(&exe)
        .arg(&src)
        .args(["-lm"])
        .output();
    let build = match build {
        Ok(b) => b,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dir);
            return format!("{{\"ok\":false,\"error\":\"компилятор не запустился: {}\"}}", esc(&e.to_string()));
        }
    };
    if !build.status.success() {
        let err = String::from_utf8_lossy(&build.stderr).to_string();
        let _ = std::fs::remove_dir_all(&dir);
        return format!(
            "{{\"ok\":false,\"error\":\"ошибка компиляции\",\"stderr\":\"{}\",\"compiler\":\"{}\"}}",
            esc(&trunc(&err)), esc(&cc)
        );
    }

    let mut child = match Command::new("timeout")
        .arg(format!("{RUN_TIMEOUT}s"))
        .arg(&exe)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dir);
            return format!("{{\"ok\":false,\"error\":\"не запустить программу: {}\"}}", esc(&e.to_string()));
        }
    };
    if let Some(mut si) = child.stdin.take() {
        let _ = si.write_all(stdin.as_bytes());
    }
    let out = child.wait_with_output();
    let _ = std::fs::remove_dir_all(&dir);
    let ms = t0.elapsed().as_millis();
    match out {
        Ok(o) => {
            let stdout = trunc(&String::from_utf8_lossy(&o.stdout));
            let stderr = trunc(&String::from_utf8_lossy(&o.stderr));
            let code = o.status.code().unwrap_or(-1);
            let note = if code == 124 {
                format!(",\"error\":\"превышен таймаут {RUN_TIMEOUT} с\"")
            } else {
                String::new()
            };
            format!(
                "{{\"ok\":true,\"stdout\":\"{}\",\"stderr\":\"{}\",\"code\":{},\"ms\":{},\"compiler\":\"{}\"{}}}",
                esc(&stdout), esc(&stderr), code, ms, esc(&cc), note
            )
        }
        Err(e) => format!("{{\"ok\":false,\"error\":\"{}\"}}", esc(&e.to_string())),
    }
}

fn trunc(s: &str) -> String {
    if s.len() <= MAX_OUT {
        s.to_string()
    } else {
        format!("{}\n… (обрезано)", &s[..s.floor_char_boundary(MAX_OUT)])
    }
}

fn find(h: &[u8], n: &[u8]) -> Option<usize> {
    h.windows(n.len()).position(|w| w == n)
}

fn nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn reply(s: &mut TcpStream, status: u16, body: &str) -> std::io::Result<()> {
    let reason = match status {
        204 => "No Content",
        200 => "OK",
        404 => "Not Found",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        _ => "OK",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json; charset=utf-8\r\n\
         Access-Control-Allow-Origin: *\r\nAccess-Control-Allow-Headers: Content-Type\r\n\
         Access-Control-Allow-Methods: POST, GET, OPTIONS\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes())?;
    s.write_all(body.as_bytes())
}

fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Достать строковое поле из плоского JSON-объекта (code/stdin).
fn json_str(src: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\"");
    let mut i = src.find(&pat)? + pat.len();
    let b = src.as_bytes();
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    if i >= b.len() || b[i] != b':' {
        return None;
    }
    i += 1;
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    if i >= b.len() || b[i] != b'"' {
        return None;
    }
    i += 1;
    let mut out = String::new();
    let mut chars = src[i..].chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                '/' => out.push('/'),
                'u' => {
                    let hex: String = (0..4).filter_map(|_| chars.next()).collect();
                    if let Ok(n) = u32::from_str_radix(&hex, 16) {
                        if let Some(ch) = char::from_u32(n) {
                            out.push(ch);
                        }
                    }
                }
                other => out.push(other),
            },
            c => out.push(c),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_fields() {
        let s = r#"{"code":"int main(){return 0;}","stdin":"5\n1 2\n"}"#;
        assert_eq!(json_str(s, "code").unwrap(), "int main(){return 0;}");
        assert_eq!(json_str(s, "stdin").unwrap(), "5\n1 2\n");
    }

    #[test]
    fn run_simple() {
        let out = run_c("#include <stdio.h>\nint main(void){int a=2,b=3;printf(\"%d\", a+b);return 0;}", "");
        assert!(out.contains("\"stdout\":\"5\""), "{out}");
    }
}
