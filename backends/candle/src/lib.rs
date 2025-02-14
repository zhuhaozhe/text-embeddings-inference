mod alibi;
#[cfg(feature = "cuda")]
mod compute_cap;
#[cfg(feature = "cuda")]
mod flash_attn;
mod layers;
mod models;

#[cfg(feature = "cuda")]
use crate::compute_cap::{
    compatible_compute_cap, get_compile_compute_cap, get_runtime_compute_cap,
};
use crate::models::{
    BertConfig, BertModel, DistilBertConfig, DistilBertModel, JinaBertModel, JinaCodeBertModel,
    Model, NomicBertModel, NomicConfig,
};
#[cfg(feature = "cuda")]
use crate::models::{
    FlashBertModel, FlashDistilBertModel, FlashJinaBertModel, FlashJinaCodeBertModel,
    FlashNomicBertModel,
};
use anyhow::Context;
use candle::{DType, Device};
use candle_nn::VarBuilder;
use nohash_hasher::BuildNoHashHasher;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use text_embeddings_backend_core::{
    Backend, BackendError, Batch, Embedding, Embeddings, ModelType, Predictions,
};

/// This enum is needed to be able to differentiate between jina models that also use
/// the `bert` model type and valid Bert models.
/// We use the `_name_or_path` field in the config to do so. This might not be robust in the long
/// run but is still better than the other options...
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "_name_or_path")]
pub enum BertConfigWrapper {
    #[serde(rename = "jinaai/jina-bert-implementation")]
    JinaBert(BertConfig),
    #[serde(rename = "jinaai/jina-bert-v2-qk-post-norm")]
    JinaCodeBert(BertConfig),
    #[serde(untagged)]
    Bert(BertConfig),
}

#[derive(Deserialize)]
#[serde(tag = "model_type", rename_all = "kebab-case")]
enum Config {
    Bert(BertConfigWrapper),
    XlmRoberta(BertConfig),
    Camembert(BertConfig),
    Roberta(BertConfig),
    #[serde(rename(deserialize = "distilbert"))]
    DistilBert(DistilBertConfig),
    #[serde(rename(deserialize = "nomic_bert"))]
    NomicBert(NomicConfig),
}

pub struct CandleBackend {
    device: Device,
    model: Box<dyn Model + Send>,
}

impl CandleBackend {
    pub fn new(
        model_path: PathBuf,
        dtype: String,
        model_type: ModelType,
    ) -> Result<Self, BackendError> {
        // Load config
        let config: String = std::fs::read_to_string(model_path.join("config.json"))
            .context("Unable to read config file")
            .map_err(|err| BackendError::Start(format!("{err:?}")))?;
        let config: Config = serde_json::from_str(&config)
            .context("Model is not supported")
            .map_err(|err| BackendError::Start(format!("{err:?}")))?;

        // Get candle device
        let device = if candle::utils::cuda_is_available() {
            #[cfg(feature = "cuda")]
            match compatible_compute_cap() {
                Ok(true) => Device::new_cuda(0),
                Ok(false) => {
                    return Err(BackendError::Start(format!(
                        "Runtime compute cap {} is not compatible with compile time compute cap {}",
                        get_runtime_compute_cap().unwrap(),
                        get_compile_compute_cap().unwrap()
                    )));
                }
                Err(err) => {
                    tracing::warn!("Could not find a compatible CUDA device on host: {err:?}");
                    tracing::warn!("Using CPU instead");
                    Ok(Device::Cpu)
                }
            }
            #[cfg(not(feature = "cuda"))]
            Ok(Device::Cpu)
        } else if candle::utils::metal_is_available() {
            Device::new_metal(0)
        } else {
            Ok(Device::Cpu)
        }
        .map_err(|err| BackendError::Start(err.to_string()))?;

        // Get candle dtype
        let dtype = if &dtype == "float32" {
            Ok(DType::F32)
        } else if &dtype == "float16" {
            Ok(DType::F16)
        } else {
            Err(BackendError::Start(format!(
                "DType {dtype} is not supported"
            )))
        }?;

        let safetensors_path = model_path.join("model.safetensors");
        let vb = if safetensors_path.exists() {
            unsafe {
                VarBuilder::from_mmaped_safetensors(
                    &[model_path.join("model.safetensors")],
                    dtype,
                    &device,
                )
            }
        } else {
            VarBuilder::from_pth(model_path.join("pytorch_model.bin"), dtype, &device)
        }
        .s()?;

        let model: Result<Box<dyn Model + Send>, BackendError> = match (config, &device) {
            #[cfg(not(feature = "cuda"))]
            (_, Device::Cuda(_)) => Err(BackendError::Start(
                "`cuda` feature is not enabled".to_string(),
            )),
            (Config::Bert(config), Device::Cpu | Device::Metal(_)) => match config {
                BertConfigWrapper::JinaBert(config) => {
                    tracing::info!("Starting JinaBertModel model on {:?}", device);
                    Ok(Box::new(JinaBertModel::load(vb, &config, model_type).s()?))
                }
                BertConfigWrapper::JinaCodeBert(config) => {
                    tracing::info!("Starting JinaCodeBert model on {:?}", device);
                    Ok(Box::new(
                        JinaCodeBertModel::load(vb, &config, model_type).s()?,
                    ))
                }
                BertConfigWrapper::Bert(config) => {
                    tracing::info!("Starting Bert model on {:?}", device);
                    Ok(Box::new(BertModel::load(vb, &config, model_type).s()?))
                }
            },
            (
                Config::XlmRoberta(config) | Config::Camembert(config) | Config::Roberta(config),
                Device::Cpu | Device::Metal(_),
            ) => {
                tracing::info!("Starting Bert model on {:?}", device);
                Ok(Box::new(
                    BertModel::load_roberta(vb, &config, model_type).s()?,
                ))
            }
            (Config::DistilBert(config), Device::Cpu | Device::Metal(_)) => {
                tracing::info!("Starting DistilBertModel model on {:?}", device);
                Ok(Box::new(
                    DistilBertModel::load(vb, &config, model_type).s()?,
                ))
            }
            (Config::NomicBert(config), Device::Cpu | Device::Metal(_)) => {
                tracing::info!("Starting NomicBertModel model on {:?}", device);
                Ok(Box::new(NomicBertModel::load(vb, &config, model_type).s()?))
            }
            #[cfg(feature = "cuda")]
            (Config::Bert(config), Device::Cuda(_)) => {
                if cfg!(any(feature = "flash-attn", feature = "flash-attn-v1"))
                    && dtype == DType::F16
                    // Allow disabling because of flash attention v1 precision problems
                    // See: https://github.com/huggingface/text-embeddings-inference/issues/37
                    && &std::env::var("USE_FLASH_ATTENTION").unwrap_or("True".to_string()).to_lowercase() == "true"
                {
                    match config {
                        BertConfigWrapper::JinaBert(config) => {
                            tracing::info!("Starting FlashJinaBert model on {:?}", device);
                            Ok(Box::new(
                                FlashJinaBertModel::load(vb, &config, model_type).s()?,
                            ))
                        }
                        BertConfigWrapper::JinaCodeBert(config) => {
                            tracing::info!("Starting FlashJinaCodeBert model on {:?}", device);
                            Ok(Box::new(
                                FlashJinaCodeBertModel::load(vb, &config, model_type).s()?,
                            ))
                        }
                        BertConfigWrapper::Bert(config) => {
                            tracing::info!("Starting FlashBert model on {:?}", device);
                            Ok(Box::new(FlashBertModel::load(vb, &config, model_type).s()?))
                        }
                    }
                } else {
                    match config {
                        BertConfigWrapper::JinaBert(config) => {
                            tracing::info!("Starting JinaBertModel model on {:?}", device);
                            Ok(Box::new(JinaBertModel::load(vb, &config, model_type).s()?))
                        }
                        BertConfigWrapper::JinaCodeBert(config) => {
                            tracing::info!("Starting JinaCodeBert model on {:?}", device);
                            Ok(Box::new(
                                JinaCodeBertModel::load(vb, &config, model_type).s()?,
                            ))
                        }
                        BertConfigWrapper::Bert(config) => {
                            tracing::info!("Starting Bert model on {:?}", device);
                            Ok(Box::new(BertModel::load(vb, &config, model_type).s()?))
                        }
                    }
                }
            }
            #[cfg(feature = "cuda")]
            (
                Config::XlmRoberta(config) | Config::Camembert(config) | Config::Roberta(config),
                Device::Cuda(_),
            ) => {
                if cfg!(any(feature = "flash-attn", feature = "flash-attn-v1"))
                    && dtype == DType::F16
                    // Allow disabling because of flash attention v1 precision problems
                    // See: https://github.com/huggingface/text-embeddings-inference/issues/37
                    && &std::env::var("USE_FLASH_ATTENTION").unwrap_or("True".to_string()).to_lowercase() == "true"
                {
                    tracing::info!("Starting FlashBert model on {:?}", device);
                    Ok(Box::new(
                        FlashBertModel::load_roberta(vb, &config, model_type).s()?,
                    ))
                } else {
                    tracing::info!("Starting Bert model on {:?}", device);
                    Ok(Box::new(
                        BertModel::load_roberta(vb, &config, model_type).s()?,
                    ))
                }
            }
            #[cfg(feature = "cuda")]
            (Config::DistilBert(config), Device::Cuda(_)) => {
                if cfg!(feature = "flash-attn")
                    && dtype == DType::F16
                    && &std::env::var("USE_FLASH_ATTENTION")
                        .unwrap_or("True".to_string())
                        .to_lowercase()
                        == "true"
                {
                    tracing::info!("Starting FlashDistilBertModel model on {:?}", device);
                    Ok(Box::new(
                        FlashDistilBertModel::load(vb, &config, model_type).s()?,
                    ))
                } else {
                    tracing::info!("Starting DistilBertModel model on {:?}", device);
                    Ok(Box::new(
                        DistilBertModel::load(vb, &config, model_type).s()?,
                    ))
                }
            }
            #[cfg(feature = "cuda")]
            (Config::NomicBert(config), Device::Cuda(_)) => {
                if cfg!(feature = "flash-attn")
                    && dtype == DType::F16
                    && &std::env::var("USE_FLASH_ATTENTION")
                        .unwrap_or("True".to_string())
                        .to_lowercase()
                        == "true"
                {
                    tracing::info!("Starting FlashNomicBertModel model on {:?}", device);
                    Ok(Box::new(
                        FlashNomicBertModel::load(vb, &config, model_type).s()?,
                    ))
                } else {
                    tracing::info!("Starting NomicBertModel model on {:?}", device);
                    Ok(Box::new(NomicBertModel::load(vb, &config, model_type).s()?))
                }
            }
        };

        Ok(Self {
            device,
            model: model?,
        })
    }
}

impl Backend for CandleBackend {
    fn max_batch_size(&self) -> Option<usize> {
        // Limit max batch size to 4 on CPU
        if matches!(self.device, Device::Cpu) {
            return Some(4);
        }
        None
    }

    fn health(&self) -> Result<(), BackendError> {
        Ok(())
    }

    fn is_padded(&self) -> bool {
        self.model.is_padded()
    }

    fn embed(&self, batch: Batch) -> Result<Embeddings, BackendError> {
        let batch_size = batch.len();
        let pooled_indices = batch.pooled_indices.clone();
        let raw_indices = batch.raw_indices.clone();

        // Used for indexing in the raw_embeddings tensor
        let input_lengths: Vec<usize> = (0..batch.len())
            .map(|i| {
                (batch.cumulative_seq_lengths[i + 1] - batch.cumulative_seq_lengths[i]) as usize
            })
            .collect();

        // Run forward
        let (pooled_embeddings, raw_embeddings) = self.model.embed(batch).e()?;

        // Device => Host data transfer
        let pooled_embeddings = match pooled_embeddings {
            None => vec![],
            Some(pooled_embeddings) => pooled_embeddings.to_dtype(DType::F32).e()?.to_vec2().e()?,
        };

        // This transfer is expensive...
        let raw_embeddings = match raw_embeddings {
            None => vec![],
            Some(raw_embeddings) => raw_embeddings.to_dtype(DType::F32).e()?.to_vec2().e()?,
        };

        let mut embeddings =
            HashMap::with_capacity_and_hasher(batch_size, BuildNoHashHasher::default());
        for (i, e) in pooled_indices.into_iter().zip(pooled_embeddings) {
            embeddings.insert(i as usize, Embedding::Pooled(e));
        }

        let mut cumulative_length = 0;
        for i in raw_indices.into_iter() {
            let length = input_lengths[i as usize];
            let e = raw_embeddings[cumulative_length..cumulative_length + length].to_vec();
            embeddings.insert(i as usize, Embedding::All(e));
            cumulative_length += length;
        }

        Ok(embeddings)
    }

    fn predict(&self, batch: Batch) -> Result<Predictions, BackendError> {
        let batch_size = batch.len();

        let results = self.model.predict(batch).e()?;
        let results = results.to_dtype(DType::F32).e()?.to_vec2().e()?;

        let mut predictions =
            HashMap::with_capacity_and_hasher(batch_size, BuildNoHashHasher::default());
        for (i, r) in results.into_iter().enumerate() {
            predictions.insert(i, r);
        }

        Ok(predictions)
    }
}

pub trait WrapErr<O> {
    fn s(self) -> Result<O, BackendError>;
    fn e(self) -> Result<O, BackendError>;
}

impl<O> WrapErr<O> for Result<O, candle::Error> {
    fn s(self) -> Result<O, BackendError> {
        self.map_err(|e| BackendError::Start(e.to_string()))
    }
    fn e(self) -> Result<O, BackendError> {
        self.map_err(|e| BackendError::Inference(e.to_string()))
    }
}
