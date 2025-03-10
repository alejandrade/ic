use crate::public_key_store::PublicKeyStore;
use crate::{
    secret_key_store::SecretKeyStore,
    vault::api::{PublicRandomSeedGenerator, PublicRandomSeedGeneratorError},
    LocalCspVault,
};
use ic_crypto_internal_logmon::metrics::{MetricsDomain, MetricsResult, MetricsScope};
use ic_crypto_internal_seed::Seed;
use rand::{CryptoRng, Rng, SeedableRng};
use rand_chacha::ChaCha20Rng;

#[cfg(test)]
mod tests;

impl<R: Rng + CryptoRng, S: SecretKeyStore, C: SecretKeyStore, P: PublicKeyStore>
    PublicRandomSeedGenerator for LocalCspVault<R, S, C, P>
{
    fn new_public_seed(&self) -> Result<Seed, PublicRandomSeedGeneratorError> {
        let start_time = self.metrics.now();
        let intermediate_seed: [u8; 32] = self.csprng.write().gen();
        let rng_for_seed_generation = &mut ChaCha20Rng::from_seed(intermediate_seed);
        let result = Ok(Seed::from_rng(rng_for_seed_generation));
        self.metrics.observe_duration_seconds(
            MetricsDomain::PublicSeed,
            MetricsScope::Local,
            "new_public_seed",
            MetricsResult::from(&result),
            start_time,
        );
        result
    }
}
