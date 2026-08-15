use ring::rand::{SecureRandom, SystemRandom};

use mycel_agent_protocol::ProviderError;

use crate::connection_error;

pub(crate) fn secure_random<const N: usize>() -> Result<[u8; N], ProviderError> {
    let mut bytes = [0_u8; N];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| connection_error("operating-system randomness is unavailable"))?;
    Ok(bytes)
}

pub(crate) fn retry_random_unit() -> f64 {
    secure_random::<8>()
        .map(u64::from_le_bytes)
        .map(|value| value as f64 / (u64::MAX as f64 + 1.0))
        // Retry jitter is not a security boundary. A stable midpoint preserves
        // bounded backoff if the operating-system RNG is temporarily absent.
        .unwrap_or(0.5)
}
