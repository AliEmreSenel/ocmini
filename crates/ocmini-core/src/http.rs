pub use ocmini_tools::http::read_body;
pub fn percent_bytes(value: &str) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    let mut input = value.as_bytes().iter().copied();
    while let Some(byte) = input.next() {
        if byte != b'%' {
            bytes.push(byte);
            continue;
        }
        let a = input
            .next()
            .and_then(|b| (b as char).to_digit(16))
            .ok_or("Invalid percent escape")?;
        let b = input
            .next()
            .and_then(|b| (b as char).to_digit(16))
            .ok_or("Invalid percent escape")?;
        bytes.push((a * 16 + b) as u8);
    }
    Ok(bytes)
}
