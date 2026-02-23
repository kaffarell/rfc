use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;

use crate::models::{Document, DocumentType, SearchFilter, SearchResult};

const DATATRACKER_BASE_URL: &str = "https://datatracker.ietf.org";

/// Client for the IETF Datatracker API
pub struct DataTrackerClient {
    client: Client,
}

/// Response from the Datatracker document search API
#[derive(Debug, Deserialize)]
struct SearchResponse {
    meta: SearchMeta,
    objects: Vec<ApiDocument>,
}

#[derive(Debug, Deserialize)]
struct SearchMeta {
    #[serde(default)]
    next: Option<String>,
}

/// Document as returned by the Datatracker API
#[derive(Debug, Deserialize)]
struct ApiDocument {
    name: String,
    title: String,
    #[serde(rename = "abstract")]
    abstract_text: Option<String>,
    pages: Option<u32>,
    #[serde(rename = "time")]
    time: Option<String>,
    #[serde(rename = "std_level")]
    std_level: Option<String>,
    stream: Option<String>,
    #[serde(default)]
    authors: Vec<String>,
}

impl DataTrackerClient {
    /// Create a new DataTracker API client
    pub fn new() -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .user_agent(concat!("rfc-cli/", env!("CARGO_PKG_VERSION")))
                .timeout(Duration::from_secs(30))
                .build()
                .context("Failed to create HTTP client")?,
        })
    }

    /// Search for documents matching the query
    /// Only returns RFCs and Internet-Drafts (filters out slides, reviews, etc.)
    pub async fn search(
        &self,
        query: &str,
        filter: SearchFilter,
        limit: u32,
    ) -> Result<SearchResult> {
        // Request more results than needed since we filter locally
        // The API returns many document types we don't want (slides, reviews, etc.)
        let api_limit = limit.saturating_mul(5);
        let encoded = urlencoding::encode(query);

        let type_suffix = filter
            .api_param()
            .map(|t| format!("&type={}", t))
            .unwrap_or_default();

        let name_url = format!(
            "{}/api/v1/doc/document/?name__icontains={}&limit={}&format=json{}",
            DATATRACKER_BASE_URL, encoded, api_limit, type_suffix
        );
        let title_url = format!(
            "{}/api/v1/doc/document/?title__icontains={}&limit={}&format=json{}",
            DATATRACKER_BASE_URL, encoded, api_limit, type_suffix
        );

        let (name_resp, title_resp) =
            tokio::try_join!(self.fetch_search(&name_url), self.fetch_search(&title_url))?;

        let has_more = name_resp.meta.next.is_some() || title_resp.meta.next.is_some();

        // Merge and deduplicate by name, preserving order (name results first)
        let mut seen = std::collections::HashSet::new();
        let documents: Vec<Document> = name_resp
            .objects
            .into_iter()
            .chain(title_resp.objects)
            .filter(|doc| Self::is_rfc_or_draft(&doc.name) && seen.insert(doc.name.clone()))
            .map(|doc| self.convert_api_document(doc))
            .take(limit as usize)
            .collect();

        let returned_count = documents.len() as u32;

        Ok(SearchResult {
            documents,
            has_more: has_more || returned_count == limit,
            query: query.to_string(),
            filter,
        })
    }

    /// Fetch a single search URL and return the parsed response
    async fn fetch_search(&self, url: &str) -> Result<SearchResponse> {
        let response = self
            .client
            .get(url)
            .send()
            .await
            .context("Failed to send search request")?;

        if !response.status().is_success() {
            anyhow::bail!(
                "Search request to {} failed: HTTP {}",
                url,
                response.status()
            );
        }

        response
            .json()
            .await
            .context("Failed to parse search response")
    }

    /// Check if a document name is an RFC or Internet-Draft
    fn is_rfc_or_draft(name: &str) -> bool {
        name.starts_with("rfc") || name.starts_with("draft-")
    }

    /// Convert an API document to our Document model
    fn convert_api_document(&self, doc: ApiDocument) -> Document {
        let doc_type = self.parse_doc_type(&doc.name);
        let published = doc.time.as_ref().and_then(|t| {
            chrono::DateTime::parse_from_rfc3339(t)
                .ok()
                .map(|dt| dt.with_timezone(&chrono::Utc))
        });

        Document {
            name: doc.name.clone(),
            title: doc.title,
            doc_type,
            abstract_text: doc.abstract_text,
            pages: doc.pages,
            published,
            status: doc.std_level,
            authors: doc.authors,
            stream: doc.stream,
            wg: None,
        }
    }

    /// Get a single document by name
    pub async fn get_document(&self, name: &str) -> Result<Document> {
        let url = format!(
            "{}/api/v1/doc/document/{}/?format=json",
            DATATRACKER_BASE_URL,
            name
        );

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .context("Failed to fetch document metadata")?;

        if !response.status().is_success() {
            anyhow::bail!("Document not found: {}", name);
        }

        let api_doc: ApiDocument = response
            .json()
            .await
            .context("Failed to parse document metadata")?;

        Ok(self.convert_api_document(api_doc))
    }

    /// Parse document type from name
    fn parse_doc_type(&self, name: &str) -> DocumentType {
        if let Some(num_str) = name.strip_prefix("rfc") {
            if let Ok(num) = num_str.parse::<u32>() {
                return DocumentType::Rfc(num);
            }
        }
        DocumentType::Draft(name.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_doc_type() {
        let client = DataTrackerClient::new().unwrap();
        assert_eq!(client.parse_doc_type("rfc9000"), DocumentType::Rfc(9000));
        assert_eq!(
            client.parse_doc_type("draft-ietf-quic-transport-34"),
            DocumentType::Draft("draft-ietf-quic-transport-34".to_string())
        );
    }
}
