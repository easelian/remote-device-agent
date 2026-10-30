use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::rand::{SecureRandom, SystemRandom};

use crate::error::{Error, Result};

pub fn bytes<const N: usize>() -> Result<[u8; N]> {
    let mut result = [0; N];
    SystemRandom::new()
        .fill(&mut result)
        .map_err(|_| Error::message("Operating-system randomness is unavailable"))?;
    Ok(result)
}

pub fn url_token<const N: usize>() -> Result<String> {
    Ok(URL_SAFE_NO_PAD.encode(bytes::<N>()?))
}

pub fn hex_token<const N: usize>() -> Result<String> {
    let bytes = bytes::<N>()?;
    let mut output = String::with_capacity(N * 2);
    use std::fmt::Write;
    for byte in bytes {
        write!(&mut output, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(output)
}
