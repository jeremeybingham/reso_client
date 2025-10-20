// metadata_parser.rs - Parse RESO $metadata XML and analyze entities

use quick_xml::events::Event;
use quick_xml::Reader;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct EntityType {
    pub name: String,
    pub properties: Vec<Property>,
}

#[derive(Debug, Clone)]
pub struct Property {
    pub name: String,
    pub property_type: String,
    pub nullable: bool,
    pub max_length: Option<i32>,
}

#[derive(Debug)]
pub struct Schema {
    pub namespace: String,
    pub entities: HashMap<String, EntityType>,
}

pub struct MetadataParser;

impl MetadataParser {
    /// Parse EDMX metadata XML and extract entity definitions
    pub fn parse(xml: &str) -> Result<Schema, Box<dyn std::error::Error>> {
        let mut reader = Reader::from_str(xml);
        reader.config_mut().trim_text(true);
        
        let mut schema = Schema {
            namespace: String::new(),
            entities: HashMap::new(),
        };
        
        let mut current_entity: Option<EntityType> = None;
        let mut buf = Vec::new();
        
        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                    match e.name().as_ref() {
                        b"Schema" => {
                            for attr in e.attributes() {
                                if let Ok(attr) = attr {
                                    if attr.key.as_ref() == b"Namespace" {
                                        schema.namespace = String::from_utf8_lossy(&attr.value).to_string();
                                    }
                                }
                            }
                        }
                        b"EntityType" => {
                            let mut entity_name = String::new();
                            for attr in e.attributes() {
                                if let Ok(attr) = attr {
                                    if attr.key.as_ref() == b"Name" {
                                        entity_name = String::from_utf8_lossy(&attr.value).to_string();
                                    }
                                }
                            }
                            current_entity = Some(EntityType {
                                name: entity_name,
                                properties: Vec::new(),
                            });
                        }
                        b"Property" => {
                            if let Some(ref mut entity) = current_entity {
                                let mut prop = Property {
                                    name: String::new(),
                                    property_type: String::new(),
                                    nullable: true,
                                    max_length: None,
                                };
                                
                                for attr in e.attributes() {
                                    if let Ok(attr) = attr {
                                        match attr.key.as_ref() {
                                            b"Name" => {
                                                prop.name = String::from_utf8_lossy(&attr.value).to_string();
                                            }
                                            b"Type" => {
                                                prop.property_type = String::from_utf8_lossy(&attr.value).to_string();
                                            }
                                            b"Nullable" => {
                                                let val = String::from_utf8_lossy(&attr.value);
                                                prop.nullable = val != "false";
                                            }
                                            b"MaxLength" => {
                                                let val = String::from_utf8_lossy(&attr.value);
                                                prop.max_length = val.parse().ok();
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                                entity.properties.push(prop);
                            }
                        }
                        _ => {}
                    }
                }
                Ok(Event::End(ref e)) => {
                    if e.name().as_ref() == b"EntityType" {
                        if let Some(entity) = current_entity.take() {
                            schema.entities.insert(entity.name.clone(), entity);
                        }
                    }
                }
                Ok(Event::Eof) => break,
                Err(e) => return Err(Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("XML parse error: {}", e)
                ))),
                _ => {}
            }
            buf.clear();
        }
        
        Ok(schema)
    }
    
    /// Generate Rust struct code from an entity definition
    pub fn generate_struct(entity: &EntityType) -> String {
        let mut code = String::new();
        
        code.push_str(&format!("/// {} entity from RESO metadata\n", entity.name));
        code.push_str("#[derive(Debug, Clone, Serialize, Deserialize)]\n");
        code.push_str(&format!("pub struct {} {{\n", entity.name));
        
        for prop in &entity.properties {
            let rust_type = Self::map_edm_to_rust(&prop.property_type);
            let field_name = Self::to_snake_case(&prop.name);
            
            code.push_str(&format!("    #[serde(rename = \"{}\")]\n", prop.name));
            
            if prop.nullable || !rust_type.starts_with('&') {
                code.push_str(&format!("    pub {}: Option<{}>,\n", field_name, rust_type));
            } else {
                code.push_str(&format!("    pub {}: {},\n", field_name, rust_type));
            }
            code.push_str("\n");
        }
        
        code.push_str("}\n");
        code
    }
    
    /// Map EDM types to Rust types
    fn map_edm_to_rust(edm_type: &str) -> String {
        match edm_type {
            "Edm.String" => "String".to_string(),
            "Edm.Int32" => "i32".to_string(),
            "Edm.Int64" => "i64".to_string(),
            "Edm.Int16" => "i16".to_string(),
            "Edm.Double" => "f64".to_string(),
            "Edm.Decimal" => "f64".to_string(),
            "Edm.Boolean" => "bool".to_string(),
            "Edm.DateTime" | "Edm.DateTimeOffset" => "chrono::DateTime<chrono::Utc>".to_string(),
            "Edm.Date" => "chrono::NaiveDate".to_string(),
            "Edm.TimeOfDay" => "chrono::NaiveTime".to_string(),
            "Edm.Guid" => "String".to_string(),
            "Edm.Binary" => "Vec<u8>".to_string(),
            _ => {
                // Check if it's a collection
                if edm_type.starts_with("Collection(") {
                    let inner = edm_type.trim_start_matches("Collection(").trim_end_matches(')');
                    format!("Vec<{}>", Self::map_edm_to_rust(inner))
                } else {
                    // Unknown type or enum reference
                    "serde_json::Value".to_string()
                }
            }
        }
    }
    
    /// Convert PascalCase to snake_case
    fn to_snake_case(s: &str) -> String {
        let mut result = String::new();
        let mut chars = s.chars().peekable();
        
        while let Some(c) = chars.next() {
            if c.is_uppercase() {
                if !result.is_empty() {
                    result.push('_');
                }
                result.push(c.to_lowercase().next().unwrap());
            } else {
                result.push(c);
            }
        }
        
        result
    }
    
    /// Print a summary of the schema
    pub fn print_summary(schema: &Schema) {
        println!("RESO Schema: {}", schema.namespace);
        println!("Total entities: {}\n", schema.entities.len());
        
        let mut entity_names: Vec<_> = schema.entities.keys().collect();
        entity_names.sort();
        
        for name in entity_names {
            if let Some(entity) = schema.entities.get(name) {
                println!("✓ {} ({} properties)", entity.name, entity.properties.len());
            }
        }
    }
    
    /// Find common RESO resources
    pub fn find_reso_resources(schema: &Schema) -> Vec<String> {
        let common_resources = vec![
            "Property",
            "Member",
            "Office",
            "OpenHouse",
            "Media",
            "Team",
            "Contact",
            "InternetAddress",
            "Contacts",
            "HistoryTransactional",
        ];
        
        common_resources
            .into_iter()
            .filter(|r| schema.entities.contains_key(*r))
            .map(|s| s.to_string())
            .collect()
    }
}