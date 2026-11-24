//! Parsed OData `$metadata`: entity sets, entity types (keys, properties, navigation properties) and enum types.
//!
//! The model is strings only (no typed Property/Member structs), so it works with any server schema. Elements the
//! parser does not know (annotations, vendor extensions, actions, complex types) are skipped without error.

use crate::error::{ResoError, Result};
use roxmltree::{Document, Node};

/// One property of an entity type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Property {
    /// Field name, e.g. `ListPrice`.
    pub name: String,
    /// Declared type, e.g. `Edm.String`, `Collection(Edm.String)` or `org.reso.metadata.enums.StandardStatus`.
    pub type_name: String,
    /// False only when the server declares `Nullable="false"` (the OData default is true).
    pub nullable: bool,
    /// `MaxLength` when declared.
    pub max_length: Option<u32>,
    /// Value of the `RESO.OData.Metadata.LookupName` annotation, when present.
    pub lookup_name: Option<String>,
}

/// A navigation property (a link to another entity type).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavigationProperty {
    /// Name, e.g. `ListAgent`.
    pub name: String,
    /// Target type, e.g. `org.reso.metadata.Member` or `Collection(org.reso.metadata.Media)`.
    pub type_name: String,
}

/// An entity type: key, properties and navigation properties.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityType {
    /// Type name without namespace, e.g. `Property`.
    pub name: String,
    /// Key property names, in declared order.
    pub key: Vec<String>,
    /// Properties in declared order.
    pub properties: Vec<Property>,
    /// Navigation properties in declared order.
    pub navigation: Vec<NavigationProperty>,
}

impl EntityType {
    /// Find a property by name.
    pub fn property(&self, name: &str) -> Option<&Property> {
        self.properties.iter().find(|p| p.name == name)
    }
}

/// An entity set (a queryable resource) and the entity type it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntitySet {
    /// Resource name used in URLs, e.g. `Property`.
    pub name: String,
    /// Full entity type name, e.g. `org.reso.metadata.Property`.
    pub entity_type: String,
}

/// One member of an enum type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumMember {
    /// Member name, e.g. `Active`.
    pub name: String,
    /// Declared integer value, when present.
    pub value: Option<i64>,
}

/// An enum (lookup) type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumType {
    /// Type name without namespace.
    pub name: String,
    /// Namespace of the schema that declares it.
    pub namespace: String,
    /// Members in declared order.
    pub members: Vec<EnumMember>,
}

/// The parsed metadata document.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Metadata {
    /// Entity sets from the `EntityContainer`; empty when the server declares none.
    pub entity_sets: Vec<EntitySet>,
    /// Entity types from every schema.
    pub entity_types: Vec<EntityType>,
    /// Enum types from every schema.
    pub enum_types: Vec<EnumType>,
}

impl Metadata {
    /// Parse a `$metadata` XML document.
    ///
    /// # Errors
    ///
    /// [`ResoError::Parse`] when the text is not well-formed XML. A well-formed document with no schema gives an
    /// empty model.
    pub fn parse(xml: &str) -> Result<Metadata> {
        let doc = Document::parse_with_options(
            xml,
            roxmltree::ParsingOptions {
                allow_dtd: true,
                ..Default::default()
            },
        )
        .map_err(|e| ResoError::Parse(format!("Invalid $metadata XML: {}", e)))?;

        let mut md = Metadata::default();
        for schema in doc.descendants().filter(|n| n.has_tag_name("Schema")) {
            let namespace = schema.attribute("Namespace").unwrap_or("").to_string();
            for el in schema.children().filter(Node::is_element) {
                match el.tag_name().name() {
                    "EntityType" => md.entity_types.push(entity_type(el)),
                    "EnumType" => md.enum_types.push(enum_type(el, &namespace)),
                    "EntityContainer" => {
                        for set in el.children().filter(|n| n.has_tag_name("EntitySet")) {
                            if let (Some(name), Some(t)) =
                                (set.attribute("Name"), set.attribute("EntityType"))
                            {
                                md.entity_sets.push(EntitySet {
                                    name: name.to_string(),
                                    entity_type: t.to_string(),
                                });
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok(md)
    }

    /// Resource names that can be queried: the entity set names, or the entity type names when the server declares
    /// no entity sets (the RESO reference server does this).
    pub fn resource_names(&self) -> Vec<&str> {
        if self.entity_sets.is_empty() {
            self.entity_types.iter().map(|t| t.name.as_str()).collect()
        } else {
            self.entity_sets.iter().map(|s| s.name.as_str()).collect()
        }
    }

    /// Entity type by name (namespace ignored).
    pub fn entity_type(&self, name: &str) -> Option<&EntityType> {
        let short = name.rsplit('.').next().unwrap_or(name);
        self.entity_types.iter().find(|t| t.name == short)
    }

    /// Entity type behind a resource: looked up through the entity set when there is one, else by name.
    pub fn resource(&self, name: &str) -> Option<&EntityType> {
        match self.entity_sets.iter().find(|s| s.name == name) {
            Some(set) => self.entity_type(&set.entity_type),
            None => self.entity_type(name),
        }
    }

    /// Enum type by name; a namespace prefix is accepted.
    pub fn enum_type(&self, name: &str) -> Option<&EnumType> {
        let short = name.rsplit('.').next().unwrap_or(name);
        self.enum_types.iter().find(|t| t.name == short)
    }
}

fn entity_type(el: Node) -> EntityType {
    let mut t = EntityType {
        name: el.attribute("Name").unwrap_or("").to_string(),
        key: Vec::new(),
        properties: Vec::new(),
        navigation: Vec::new(),
    };
    for c in el.children().filter(Node::is_element) {
        match c.tag_name().name() {
            "Key" => t.key.extend(
                c.children()
                    .filter(|n| n.has_tag_name("PropertyRef"))
                    .filter_map(|n| n.attribute("Name"))
                    .map(str::to_string),
            ),
            "Property" => t.properties.push(Property {
                name: c.attribute("Name").unwrap_or("").to_string(),
                type_name: c.attribute("Type").unwrap_or("").to_string(),
                nullable: c.attribute("Nullable") != Some("false"),
                max_length: c.attribute("MaxLength").and_then(|v| v.parse().ok()),
                lookup_name: c
                    .children()
                    .find(|n| {
                        n.has_tag_name("Annotation")
                            && n.attribute("Term") == Some("RESO.OData.Metadata.LookupName")
                    })
                    .and_then(|n| n.attribute("String"))
                    .map(str::to_string),
            }),
            "NavigationProperty" => t.navigation.push(NavigationProperty {
                name: c.attribute("Name").unwrap_or("").to_string(),
                type_name: c.attribute("Type").unwrap_or("").to_string(),
            }),
            _ => {}
        }
    }
    t
}

fn enum_type(el: Node, namespace: &str) -> EnumType {
    EnumType {
        name: el.attribute("Name").unwrap_or("").to_string(),
        namespace: namespace.to_string(),
        members: el
            .children()
            .filter(|n| n.has_tag_name("Member"))
            .map(|m| EnumMember {
                name: m.attribute("Name").unwrap_or("").to_string(),
                value: m.attribute("Value").and_then(|v| v.parse().ok()),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Invented, in the style of the RESO reference server (no entity container).
    const REFERENCE_STYLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="org.example" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Property">
        <Key><PropertyRef Name="ListingKey"/></Key>
        <Property Name="ListingKey" Type="Edm.String" Nullable="false" MaxLength="255"/>
        <Property Name="ListPrice" Type="Edm.Decimal" Precision="14" Scale="2"/>
        <Property Name="Appliances" Type="Collection(Edm.String)" MaxLength="1024">
          <Annotation Term="RESO.OData.Metadata.LookupName" String="Appliances"/>
        </Property>
        <NavigationProperty Name="ListAgent" Type="org.example.Member"/>
        <NavigationProperty Name="Media" Type="Collection(org.example.Media)"/>
      </EntityType>
      <EntityType Name="Member">
        <Key><PropertyRef Name="MemberKey"/></Key>
        <Property Name="MemberKey" Type="Edm.String" Nullable="false"/>
      </EntityType>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

    /// Invented edge cases: entity container, enum types in a second schema, vendor elements, composite key.
    const EDGE_CASES: &str = r#"<?xml version="1.0"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="org.example" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Listing" vendor:Flag="x" xmlns:vendor="urn:vendor">
        <Key><PropertyRef Name="A"/><PropertyRef Name="B"/></Key>
        <Property Name="A" Type="Edm.String"/>
        <Property Name="B" Type="Edm.Int64" Nullable="false"/>
        <Property Name="Status" Type="org.example.enums.Status"/>
        <vendor:Extra><Deep/></vendor:Extra>
      </EntityType>
      <ComplexType Name="Ignored"><Property Name="Z" Type="Edm.String"/></ComplexType>
      <Action Name="DoIt"/>
      <EntityContainer Name="Default">
        <EntitySet Name="Listings" EntityType="org.example.Listing"/>
        <Singleton Name="One" Type="org.example.Listing"/>
      </EntityContainer>
    </Schema>
    <Schema Namespace="org.example.enums">
      <EnumType Name="Status">
        <Member Name="Active" Value="1"/>
        <Member Name="Closed" Value="2"><Annotation Term="x" String="y"/></Member>
        <Member Name="NoValue"/>
      </EnumType>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

    #[test]
    fn parses_types_keys_properties_and_navigation() {
        let md = Metadata::parse(REFERENCE_STYLE).unwrap();
        assert!(md.entity_sets.is_empty());
        assert_eq!(md.resource_names(), vec!["Property", "Member"]);
        let p = md.resource("Property").unwrap();
        assert_eq!(p.key, vec!["ListingKey"]);
        let key = p.property("ListingKey").unwrap();
        assert!(!key.nullable);
        assert_eq!(key.max_length, Some(255));
        assert!(p.property("ListPrice").unwrap().nullable);
        let a = p.property("Appliances").unwrap();
        assert_eq!(a.type_name, "Collection(Edm.String)");
        assert_eq!(a.lookup_name.as_deref(), Some("Appliances"));
        assert_eq!(p.navigation.len(), 2);
        assert_eq!(p.navigation[1].type_name, "Collection(org.example.Media)");
    }

    #[test]
    fn parses_sets_enums_composite_keys_and_skips_unknown_elements() {
        let md = Metadata::parse(EDGE_CASES).unwrap();
        assert_eq!(md.resource_names(), vec!["Listings"]);
        assert_eq!(md.entity_sets[0].entity_type, "org.example.Listing");
        let t = md.resource("Listings").unwrap();
        assert_eq!(t.key, vec!["A", "B"]);
        assert_eq!(t.properties.len(), 3);
        let e = md.enum_type("org.example.enums.Status").unwrap();
        assert_eq!(e.namespace, "org.example.enums");
        assert_eq!(e.members.len(), 3);
        assert_eq!(e.members[1].value, Some(2));
        assert_eq!(e.members[2].value, None);
    }

    #[test]
    fn empty_schema_gives_empty_model_and_bad_xml_errors() {
        let md = Metadata::parse("<edmx:Edmx xmlns:edmx=\"urn:x\"/>").unwrap();
        assert_eq!(md, Metadata::default());
        assert!(matches!(Metadata::parse("<oops"), Err(ResoError::Parse(_))));
    }
}
