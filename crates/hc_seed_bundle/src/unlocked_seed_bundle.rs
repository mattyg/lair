use one_err::*;
use sodoken::SodokenResult;

use std::future::Future;
use std::sync::Arc;

/// The hcSeedBundle spec specifies a fixed KDF context of b"SeedBndl".
const KDF_CONTEXT: &[u8; 8] = b"SeedBndl";

/// This is the main struct for interacting with SeedBundles.
///
/// To create an [UnlockedSeedBundle]:
/// - A new random bundle: [UnlockedSeedBundle::new_random]
/// - Derived from an existing bundle: [UnlockedSeedBundle::derive]
/// - Unlock encrypted bundle bytes: [UnlockedSeedBundle::from_locked]
///
/// Once unlocked, you can get or set associated app data, or sign messages.
///
/// To "lock" (generate encrypted binary seed bundle representation), use
/// [UnlockedSeedBundle::lock] and supply the desired SeedCiphers.
#[derive(Clone)]
pub struct UnlockedSeedBundle {
    seed: sodoken::BufReadSized<32>,
    sign_pub_key: sodoken::BufReadSized<{ sodoken::sign::PUBLICKEYBYTES }>,
    sign_sec_key: sodoken::BufReadSized<{ sodoken::sign::SECRETKEYBYTES }>,
    app_data: Arc<[u8]>,
}

impl UnlockedSeedBundle {
    /// Private core constructor
    pub(crate) async fn priv_from_seed(
        seed: sodoken::BufReadSized<32>,
    ) -> SodokenResult<Self> {
        // generate the deterministic signature keypair represented by this seed
        let pk = sodoken::BufWriteSized::new_no_lock();
        let sk = sodoken::BufWriteSized::new_mem_locked()?;
        sodoken::sign::seed_keypair(pk.clone(), sk.clone(), seed.clone())
            .await?;

        // generate the full struct bundle with blank app_data
        Ok(Self {
            seed,
            sign_pub_key: pk.to_read_sized(),
            sign_sec_key: sk.to_read_sized(),
            app_data: Arc::new([]),
        })
    }

    /// Construct a new random seed SeedBundle.
    pub async fn new_random() -> SodokenResult<Self> {
        let seed = sodoken::BufWriteSized::new_mem_locked()?;
        sodoken::random::bytes_buf(seed.clone()).await?;
        Self::priv_from_seed(seed.to_read_sized()).await
    }

    /// Decode locked SeedBundle bytes into a list of
    /// LockedSeedCiphers to be used for decrypting the bundle.
    pub async fn from_locked(
        bytes: &[u8],
    ) -> SodokenResult<Vec<crate::LockedSeedCipher>> {
        crate::LockedSeedCipher::from_locked(bytes)
    }

    /// Get the actual seed tracked by this seed bundle.
    pub fn get_seed(&self) -> sodoken::BufReadSized<32> {
        self.seed.clone()
    }

    /// Derive a new sub SeedBundle by given index.
    pub fn derive(
        &self,
        index: u32,
    ) -> impl Future<Output = SodokenResult<Self>> + 'static + Send {
        let seed = self.seed.clone();
        async move {
            let new_seed = sodoken::BufWriteSized::new_mem_locked()?;
            sodoken::kdf::derive_from_key(
                new_seed.clone(),
                index as u64,
                *KDF_CONTEXT,
                seed,
            )?;
            Self::priv_from_seed(new_seed.to_read_sized()).await
        }
    }

    /// Get the signature pub key generated by this seed.
    pub fn get_sign_pub_key(
        &self,
    ) -> sodoken::BufReadSized<{ sodoken::sign::PUBLICKEYBYTES }> {
        self.sign_pub_key.clone()
    }

    /// Sign some data with the secret key generated by this seed.
    pub fn sign_detached<M>(
        &self,
        message: M,
    ) -> impl Future<
        Output = SodokenResult<sodoken::BufReadSized<{ sodoken::sign::BYTES }>>,
    >
           + 'static
           + Send
    where
        M: Into<sodoken::BufRead> + 'static + Send,
    {
        let sign_sec_key = self.sign_sec_key.clone();
        async move {
            let sig = sodoken::BufWriteSized::new_no_lock();
            sodoken::sign::detached(sig.clone(), message, sign_sec_key).await?;
            Ok(sig.to_read_sized())
        }
    }

    /// Get the raw appData bytes.
    pub fn get_app_data_bytes(&self) -> &[u8] {
        &self.app_data
    }

    /// Set the raw appData bytes.
    pub fn set_app_data_bytes<B>(&mut self, app_data: B)
    where
        B: Into<Arc<[u8]>>,
    {
        self.app_data = app_data.into();
    }

    /// Get the decoded appData bytes by type.
    pub fn get_app_data<T>(&self) -> SodokenResult<T>
    where
        T: 'static + for<'de> serde::Deserialize<'de>,
    {
        rmp_serde::from_slice(&self.app_data).map_err(OneErr::new)
    }

    /// Set the encoded appData bytes by type.
    pub fn set_app_data<T>(&mut self, t: &T) -> SodokenResult<()>
    where
        T: serde::Serialize,
    {
        let mut se =
            rmp_serde::encode::Serializer::new(Vec::new()).with_struct_map();
        t.serialize(&mut se).map_err(OneErr::new)?;
        self.app_data = se.into_inner().into_boxed_slice().into();
        Ok(())
    }

    /// Get a SeedCipherBuilder that will allow us to lock this bundle.
    pub fn lock(&self) -> crate::SeedCipherBuilder {
        crate::SeedCipherBuilder::new(self.seed.clone(), self.app_data.clone())
    }
}

#[cfg(test)]
mod tests {
    use crate::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn test_pwhash_cipher() {
        let mut seed = UnlockedSeedBundle::new_random().await.unwrap();
        seed.set_app_data(&42_isize).unwrap();

        let orig_pub_key = seed.get_sign_pub_key();

        let passphrase = sodoken::BufRead::from(b"test-passphrase".to_vec());

        let cipher = PwHashLimits::Minimum
            .with_exec(|| seed.lock().add_pwhash_cipher(passphrase.clone()));

        let encoded = cipher.lock().await.unwrap();

        match UnlockedSeedBundle::from_locked(&encoded)
            .await
            .unwrap()
            .remove(0)
        {
            LockedSeedCipher::PwHash(cipher) => {
                let seed = cipher.unlock(passphrase).await.unwrap();
                assert_eq!(
                    &*orig_pub_key.read_lock(),
                    &*seed.get_sign_pub_key().read_lock()
                );
                assert_eq!(42, seed.get_app_data::<isize>().unwrap());
            }
            oth => panic!("unexpected cipher: {:?}", oth),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_security_questions_cipher() {
        let mut seed = UnlockedSeedBundle::new_random().await.unwrap();
        seed.set_app_data(&42_isize).unwrap();

        let orig_pub_key = seed.get_sign_pub_key();

        let q1 = "What Color?";
        let q2 = "What Flavor?";
        let q3 = "What Hair?";
        let a1 = sodoken::BufRead::from(b"blUe".to_vec());
        let a2 = sodoken::BufRead::from(b"spicy ".to_vec());
        let a3 = sodoken::BufRead::from(b" big".to_vec());

        let cipher = PwHashLimits::Minimum.with_exec(|| {
            let q_list = (q1.to_string(), q2.to_string(), q3.to_string());
            let a_list = (a1, a2, a3);
            seed.lock().add_security_question_cipher(q_list, a_list)
        });

        let encoded = cipher.lock().await.unwrap();

        match UnlockedSeedBundle::from_locked(&encoded)
            .await
            .unwrap()
            .remove(0)
        {
            LockedSeedCipher::SecurityQuestions(cipher) => {
                assert_eq!(q1, cipher.get_question_list().0);
                assert_eq!(q2, cipher.get_question_list().1);
                assert_eq!(q3, cipher.get_question_list().2);

                let a1 = sodoken::BufRead::from(b" blue".to_vec());
                let a2 = sodoken::BufRead::from(b" spicy".to_vec());
                let a3 = sodoken::BufRead::from(b" bIg".to_vec());

                let seed = cipher.unlock((a1, a2, a3)).await.unwrap();

                assert_eq!(
                    &*orig_pub_key.read_lock(),
                    &*seed.get_sign_pub_key().read_lock()
                );
                assert_eq!(42, seed.get_app_data::<isize>().unwrap());
            }
            oth => panic!("unexpected cipher: {:?}", oth),
        }
    }
}
