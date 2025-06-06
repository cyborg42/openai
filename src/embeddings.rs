//! Get a vector representation of a given input that can be easily consumed by machine learning models and algorithms.
//!
//! Related guide: [Embeddings](https://beta.openai.com/docs/guides/embeddings)

use super::{openai_post, ApiResponseOrError, Credentials};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Clone)]
struct CreateEmbeddingsRequestBody<'a> {
    model: &'a str,
    input: Vec<&'a str>,
    #[serde(skip_serializing_if = "str::is_empty")]
    user: &'a str,
}

#[derive(Deserialize, Clone)]
pub struct Embeddings {
    pub data: Vec<Embedding>,
    pub model: String,
    pub usage: EmbeddingsUsage,
}

#[derive(Deserialize, Clone, Copy)]
pub struct EmbeddingsUsage {
    pub prompt_tokens: u32,
    pub total_tokens: u32,
}

#[derive(Deserialize, Clone)]
pub struct Embedding {
    #[serde(rename = "embedding")]
    pub vec: Vec<f64>,
}

impl Embeddings {
    /// Creates an embedding vector representing the input text.
    ///
    /// # Arguments
    ///
    /// * `model` - ID of the model to use.
    ///   You can use the [List models](https://beta.openai.com/docs/api-reference/models/list)
    ///   API to see all of your available models, or see our [Model overview](https://beta.openai.com/docs/models/overview)
    ///   for descriptions of them.
    /// * `input` - Input text to get embeddings for, encoded as a string or array of tokens.
    ///   To get embeddings for multiple inputs in a single request, pass an array of strings or array of token arrays.
    ///   Each input must not exceed 8192 tokens in length.
    /// * `user` - A unique identifier representing your end-user, which can help OpenAI to monitor and detect abuse.
    ///   [Learn more](https://beta.openai.com/docs/guides/safety-best-practices/end-user-ids).
    /// * `credentials` - The OpenAI credentials.
    pub async fn create(
        model: &str,
        input: Vec<&str>,
        user: &str,
        credentials: Credentials,
    ) -> ApiResponseOrError<Self> {
        openai_post(
            "embeddings",
            &CreateEmbeddingsRequestBody { model, input, user },
            Some(credentials),
        )
        .await
    }

    pub fn distances(&self) -> Vec<f64> {
        let mut distances = Vec::new();
        let mut last_embedding: Option<&Embedding> = None;

        for embedding in &self.data {
            if let Some(other) = last_embedding {
                distances.push(embedding.distance(other));
            }

            last_embedding = Some(embedding);
        }

        distances
    }
}

impl Embedding {
    pub async fn create(
        model: &str,
        input: &str,
        user: &str,
        credentials: Credentials,
    ) -> ApiResponseOrError<Self> {
        let mut embeddings = Embeddings::create(model, vec![input], user, credentials).await?;
        Ok(embeddings.data.swap_remove(0))
    }

    pub fn magnitude(&self) -> f64 {
        self.vec.iter().map(|x| x * x).sum::<f64>().sqrt()
    }

    pub fn distance(&self, other: &Self) -> f64 {
        let dot_product: f64 = self
            .vec
            .iter()
            .zip(other.vec.iter())
            .map(|(x, y)| x * y)
            .sum();
        let product_of_magnitudes = self.magnitude() * other.magnitude();

        1.0 - dot_product / product_of_magnitudes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dotenvy::dotenv;

    #[tokio::test]
    async fn embeddings() {
        dotenv().ok();
        let credentials = Credentials::from_env();

        let embeddings = Embeddings::create(
            "text-embedding-ada-002",
            vec!["The food was delicious and the waiter..."],
            "",
            credentials,
        )
        .await
        .unwrap();

        assert!(!embeddings.data.first().unwrap().vec.is_empty());
    }

    #[tokio::test]
    async fn embedding() {
        dotenv().ok();
        let credentials = Credentials::from_env();

        let embedding = Embedding::create(
            "text-embedding-ada-002",
            "The food was delicious and the waiter...",
            "",
            credentials,
        )
        .await
        .unwrap();

        assert!(!embedding.vec.is_empty());
    }

    #[test]
    fn right_angle() {
        let embeddings = Embeddings {
            data: vec![
                Embedding {
                    vec: vec![1.0, 0.0, 0.0],
                },
                Embedding {
                    vec: vec![0.0, 1.0, 0.0],
                },
            ],
            model: "text-embedding-ada-002".to_string(),
            usage: EmbeddingsUsage {
                prompt_tokens: 0,
                total_tokens: 0,
            },
        };
        assert_eq!(embeddings.distances()[0], 1.0);
    }

    #[test]
    fn non_right_angle() {
        let embeddings = Embeddings {
            data: vec![
                Embedding {
                    vec: vec![1.0, 1.0, 0.0],
                },
                Embedding {
                    vec: vec![0.0, 1.0, 0.0],
                },
            ],
            model: "text-embedding-ada-002".to_string(),
            usage: EmbeddingsUsage {
                prompt_tokens: 0,
                total_tokens: 0,
            },
        };

        assert_eq!(embeddings.distances()[0], 0.29289321881345254);
    }
}
