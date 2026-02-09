//! Configuration types for client positioning
//!
//! This module defines the types used for configuring client positions
//! relative to the server or other clients.

use anyhow::Result;

/// Position of a client relative to another client or the server
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(clippy::enum_variant_names)]
pub enum Position {
    /// Client is to the left of the reference
    LeftOf,
    /// Client is to the right of the reference
    RightOf,
    /// Client is above the reference
    TopOf,
    /// Client is below the reference
    BottomOf,
}

impl Position {
    /// Parse a position string into a Position enum
    ///
    /// # Arguments
    ///
    /// * `s` - The position string ("leftof", "rightof", "topof", or "bottomof")
    ///
    /// # Returns
    ///
    /// The corresponding Position enum value
    ///
    /// # Errors
    ///
    /// Returns an error if the string is not a valid position
    pub fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "leftof" => Ok(Position::LeftOf),
            "rightof" => Ok(Position::RightOf),
            "topof" => Ok(Position::TopOf),
            "bottomof" => Ok(Position::BottomOf),
            _ => anyhow::bail!(
                "Invalid position '{}'. Must be one of: leftof, rightof, topof, bottomof",
                s
            ),
        }
    }

    /// Convert to schengen::server::Position
    ///
    /// Maps our "X of Y" naming to schengen's "position relative to server" naming:
    /// - LeftOf => Left (client is to the left of the reference)
    /// - RightOf => Right (client is to the right of the reference)
    /// - TopOf => Above (client is above the reference)
    /// - BottomOf => Below (client is below the reference)
    pub fn to_schengen(self) -> schengen::server::Position {
        match self {
            Position::LeftOf => schengen::server::Position::Left,
            Position::RightOf => schengen::server::Position::Right,
            Position::TopOf => schengen::server::Position::Above,
            Position::BottomOf => schengen::server::Position::Below,
        }
    }
}

/// Configuration for a client
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientConfig {
    /// The client's name
    pub name: String,
    /// The client's position relative to the reference
    pub position: Position,
    /// The reference point (either "self" for the server, or another client's name)
    pub reference: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_position_from_str_valid() {
        assert_eq!(Position::from_str("leftof").unwrap(), Position::LeftOf);
        assert_eq!(Position::from_str("rightof").unwrap(), Position::RightOf);
        assert_eq!(Position::from_str("topof").unwrap(), Position::TopOf);
        assert_eq!(Position::from_str("bottomof").unwrap(), Position::BottomOf);
    }

    #[test]
    fn test_position_from_str_case_insensitive() {
        assert_eq!(Position::from_str("LeftOf").unwrap(), Position::LeftOf);
        assert_eq!(Position::from_str("RIGHTOF").unwrap(), Position::RightOf);
        assert_eq!(Position::from_str("TopOf").unwrap(), Position::TopOf);
        assert_eq!(Position::from_str("BOTTOMOF").unwrap(), Position::BottomOf);
    }

    #[test]
    fn test_position_from_str_invalid() {
        assert!(Position::from_str("invalid").is_err());
        assert!(Position::from_str("left").is_err());
        assert!(Position::from_str("").is_err());
    }

    #[test]
    fn test_position_to_schengen() {
        assert_eq!(
            Position::LeftOf.to_schengen(),
            schengen::server::Position::Left
        );
        assert_eq!(
            Position::RightOf.to_schengen(),
            schengen::server::Position::Right
        );
        assert_eq!(
            Position::TopOf.to_schengen(),
            schengen::server::Position::Above
        );
        assert_eq!(
            Position::BottomOf.to_schengen(),
            schengen::server::Position::Below
        );
    }
}
