use roxmltree::Document;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum NavicatError {
    #[error("XML Parsing Error: {0}")]
    XmlError(#[from] roxmltree::Error),
    #[error("Invalid or unsupported Navicat format")]
    InvalidFormat,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NavicatConnection {
    pub name: String,
    pub conn_type: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password_encrypted: String,
    // Note: For MVP, we extract the encrypted password.
    // Decrypting Navicat's AES/Blowfish requires specific hardcoded keys
    // which is complex for a quick MVP. We'll return it and let the user re-type
    // or provide a basic stub for now.
}

pub struct NavicatParser;

impl NavicatParser {
    /// Parses an exported .ncx XML file content and extracts MySQL connections
    pub fn parse_ncx(xml_content: &str) -> Result<Vec<NavicatConnection>, NavicatError> {
        let doc = Document::parse(xml_content)?;
        let mut connections = Vec::new();

        for node in doc.descendants() {
            if node.has_tag_name("Connection") {
                let conn_type = node.attribute("ConnType").unwrap_or("").to_string();

                // For this SQL Assistant, we mainly care about MySQL/MariaDB
                if conn_type.eq_ignore_ascii_case("MySQL")
                    || conn_type.eq_ignore_ascii_case("MariaDB")
                {
                    let name = node
                        .attribute("ConnectionName")
                        .unwrap_or("Unknown")
                        .to_string();
                    let host = node.attribute("Host").unwrap_or("127.0.0.1").to_string();
                    let port_str = node.attribute("Port").unwrap_or("3306");
                    let port = port_str.parse::<u16>().unwrap_or(3306);
                    let username = node.attribute("UserName").unwrap_or("root").to_string();
                    let password_encrypted = node.attribute("Password").unwrap_or("").to_string();

                    connections.push(NavicatConnection {
                        name,
                        conn_type,
                        host,
                        port,
                        username,
                        password_encrypted,
                    });
                }
            }
        }

        Ok(connections)
    }
}
