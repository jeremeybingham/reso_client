// Integration tests for query building

use reso_client::{Query, QueryBuilder, ReplicationQueryBuilder, ResoError};

#[test]
fn test_query_builder_basic() {
    let query = QueryBuilder::new("Property").top(10).build().unwrap();

    assert_eq!(query.to_odata_string(), "Property?$top=10");
}

#[test]
fn test_query_resource_only() {
    let query = QueryBuilder::new("Property").build().unwrap();

    assert_eq!(query.to_odata_string(), "Property");
}

#[test]
fn test_query_with_filter() {
    let query = QueryBuilder::new("Property")
        .filter("City eq 'Austin'")
        .build()
        .unwrap();

    let url = query.to_odata_string();
    assert!(url.starts_with("Property?"));
    assert!(url.contains("$filter=City%20eq%20%27Austin%27"));
}

#[test]
fn test_query_with_select() {
    let query = QueryBuilder::new("Property")
        .select(&["ListingKey", "City", "ListPrice"])
        .build()
        .unwrap();

    let url = query.to_odata_string();
    assert!(url.contains("$select=ListingKey,City,ListPrice"));
}

#[test]
fn test_query_with_orderby() {
    let query = QueryBuilder::new("Property")
        .order_by("ListPrice", "desc")
        .build()
        .unwrap();

    let url = query.to_odata_string();
    assert!(url.contains("$orderby=ListPrice%20desc"));
}

#[test]
fn test_query_with_skip() {
    let query = QueryBuilder::new("Property").skip(20).build().unwrap();

    let url = query.to_odata_string();
    assert!(url.contains("$skip=20"));
}

#[test]
fn test_query_with_count() {
    let query = QueryBuilder::new("Property").with_count().build().unwrap();

    let url = query.to_odata_string();
    assert!(url.contains("$count=true"));
}

#[test]
fn test_query_with_multiple_params() {
    let query = QueryBuilder::new("Property")
        .filter("City eq 'Austin'")
        .select(&["ListingKey", "City"])
        .top(5)
        .skip(10)
        .order_by("ListPrice", "desc")
        .with_count()
        .build()
        .unwrap();

    let url = query.to_odata_string();

    // Verify all parameters are present
    assert!(url.starts_with("Property?"));
    assert!(url.contains("$filter="));
    assert!(url.contains("$select=ListingKey,City"));
    assert!(url.contains("$top=5"));
    assert!(url.contains("$skip=10"));
    assert!(url.contains("$orderby="));
    assert!(url.contains("$count=true"));
}

#[test]
fn test_query_filter_url_encoding() {
    let query = QueryBuilder::new("Property")
        .filter("City eq 'San Francisco' and ListPrice gt 1000000")
        .build()
        .unwrap();

    let url = query.to_odata_string();
    // Spaces and quotes should be URL encoded
    assert!(url.contains("%20")); // Space encoded
    assert!(url.contains("%27")); // Single quote encoded
}

#[test]
fn test_query_complex_filter() {
    let query = QueryBuilder::new("Property")
        .filter("(City eq 'Austin' or City eq 'Dallas') and ListPrice gt 500000")
        .build()
        .unwrap();

    let url = query.to_odata_string();
    assert!(url.contains("$filter="));
    assert!(url.contains("Austin"));
}

#[test]
fn test_query_direct_construction() {
    let query = Query::new("Member");
    assert_eq!(query.to_odata_string(), "Member");
}

#[test]
fn test_query_pagination() {
    // First page
    let query1 = QueryBuilder::new("Property").top(20).build().unwrap();
    assert_eq!(query1.to_odata_string(), "Property?$top=20");

    // Second page
    let query2 = QueryBuilder::new("Property")
        .skip(20)
        .top(20)
        .build()
        .unwrap();
    let url = query2.to_odata_string();
    assert!(url.contains("$skip=20"));
    assert!(url.contains("$top=20"));
}

// Replication query tests

#[test]
fn test_replication_query_basic() {
    let query = ReplicationQueryBuilder::new("Property")
        .top(2000)
        .build()
        .unwrap();

    assert_eq!(query.to_odata_string(), "Property/replication?$top=2000");
}

#[test]
fn test_replication_query_resource_only() {
    let query = ReplicationQueryBuilder::new("Property").build().unwrap();

    assert_eq!(query.to_odata_string(), "Property/replication");
}

#[test]
fn test_replication_query_with_filter() {
    let query = ReplicationQueryBuilder::new("Property")
        .filter("StandardStatus eq 'Active'")
        .build()
        .unwrap();

    let url = query.to_odata_string();
    assert!(url.starts_with("Property/replication?"));
    assert!(url.contains("$filter=StandardStatus%20eq%20%27Active%27"));
}

#[test]
fn test_replication_query_with_select() {
    let query = ReplicationQueryBuilder::new("Property")
        .select(&["ListingKey", "City", "ListPrice"])
        .build()
        .unwrap();

    let url = query.to_odata_string();
    assert!(url.contains("$select=ListingKey,City,ListPrice"));
}

#[test]
fn test_replication_query_with_multiple_params() {
    let query = ReplicationQueryBuilder::new("Property")
        .filter("City eq 'Austin'")
        .select(&["ListingKey", "City"])
        .top(1000)
        .build()
        .unwrap();

    let url = query.to_odata_string();

    // Verify all parameters are present
    assert!(url.starts_with("Property/replication?"));
    assert!(url.contains("$filter="));
    assert!(url.contains("$select=ListingKey,City"));
    assert!(url.contains("$top=1000"));
}

#[test]
fn test_replication_query_top_limit_validation() {
    // Should succeed with 2000
    let query = ReplicationQueryBuilder::new("Property").top(2000).build();
    assert!(query.is_ok());

    // Should fail with 2001
    let query = ReplicationQueryBuilder::new("Property").top(2001).build();
    assert!(query.is_err());

    if let Err(e) = query {
        assert!(matches!(e, ResoError::InvalidQuery(_)));
    }
}

#[test]
fn test_replication_query_url_encoding() {
    let query = ReplicationQueryBuilder::new("Property")
        .filter("City eq 'San Francisco' and ListPrice gt 1000000")
        .build()
        .unwrap();

    let url = query.to_odata_string();
    // Spaces and quotes should be URL encoded
    assert!(url.contains("%20")); // Space encoded
    assert!(url.contains("%27")); // Single quote encoded
}

// Expand tests

#[test]
fn test_query_with_expand_single() {
    let query = QueryBuilder::new("Property")
        .expand(&["ListOffice"])
        .build()
        .unwrap();

    let url = query.to_odata_string();
    assert!(url.contains("$expand=ListOffice"));
}

#[test]
fn test_query_with_expand_multiple() {
    let query = QueryBuilder::new("Property")
        .expand(&["ListOffice", "ListAgent"])
        .build()
        .unwrap();

    let url = query.to_odata_string();
    assert!(url.contains("$expand=ListOffice,ListAgent"));
}

#[test]
fn test_query_with_expand_and_select() {
    let query = QueryBuilder::new("Property")
        .select(&["ListingKey", "City", "ListPrice", "ListOffice", "ListAgent"])
        .expand(&["ListOffice", "ListAgent"])
        .build()
        .unwrap();

    let url = query.to_odata_string();
    assert!(url.contains("$select=ListingKey,City,ListPrice,ListOffice,ListAgent"));
    assert!(url.contains("$expand=ListOffice,ListAgent"));
}

#[test]
fn test_query_with_expand_and_filter() {
    let query = QueryBuilder::new("Property")
        .filter("City eq 'Austin'")
        .expand(&["ListOffice"])
        .top(10)
        .build()
        .unwrap();

    let url = query.to_odata_string();
    assert!(url.contains("$filter=City%20eq%20%27Austin%27"));
    assert!(url.contains("$expand=ListOffice"));
    assert!(url.contains("$top=10"));
}

// Key access tests

#[test]
fn test_key_access_basic() {
    let query = QueryBuilder::by_key("Property", "12345").build().unwrap();

    assert_eq!(query.to_odata_string(), "Property('12345')");
}

#[test]
fn test_key_access_with_select() {
    let query = QueryBuilder::by_key("Property", "12345")
        .select(&["ListingKey", "City", "ListPrice"])
        .build()
        .unwrap();

    let url = query.to_odata_string();
    assert!(url.starts_with("Property('12345')?"));
    assert!(url.contains("$select=ListingKey,City,ListPrice"));
}

#[test]
fn test_key_access_with_expand() {
    let query = QueryBuilder::by_key("Property", "12345")
        .expand(&["ListOffice", "ListAgent"])
        .build()
        .unwrap();

    let url = query.to_odata_string();
    assert!(url.starts_with("Property('12345')?"));
    assert!(url.contains("$expand=ListOffice,ListAgent"));
}

#[test]
fn test_key_access_with_select_and_expand() {
    let query = QueryBuilder::by_key("Property", "12345")
        .select(&["ListingKey", "City", "ListOffice", "ListAgent"])
        .expand(&["ListOffice", "ListAgent"])
        .build()
        .unwrap();

    let url = query.to_odata_string();
    assert!(url.starts_with("Property('12345')?"));
    assert!(url.contains("$select=ListingKey,City,ListOffice,ListAgent"));
    assert!(url.contains("$expand=ListOffice,ListAgent"));
}

#[test]
fn test_key_access_url_encoding() {
    let query = QueryBuilder::by_key("Property", "ABC-123 456")
        .build()
        .unwrap();

    let url = query.to_odata_string();
    // Spaces should be URL encoded
    assert!(url.contains("ABC-123%20456"));
}

#[test]
fn test_key_access_special_characters() {
    let query = QueryBuilder::by_key("Property", "key/with/slashes")
        .build()
        .unwrap();

    let url = query.to_odata_string();
    // Slashes should be URL encoded
    assert!(url.contains("%2F"));
}

#[test]
fn test_key_access_rejects_filter() {
    let result = QueryBuilder::by_key("Property", "12345")
        .filter("City eq 'Austin'")
        .build();

    assert!(result.is_err());
    if let Err(e) = result {
        assert!(matches!(e, ResoError::InvalidQuery(_)));
    }
}

#[test]
fn test_key_access_rejects_top() {
    let result = QueryBuilder::by_key("Property", "12345").top(10).build();

    assert!(result.is_err());
    if let Err(e) = result {
        assert!(matches!(e, ResoError::InvalidQuery(_)));
    }
}

#[test]
fn test_key_access_rejects_skip() {
    let result = QueryBuilder::by_key("Property", "12345").skip(20).build();

    assert!(result.is_err());
    if let Err(e) = result {
        assert!(matches!(e, ResoError::InvalidQuery(_)));
    }
}

#[test]
fn test_key_access_rejects_orderby() {
    let result = QueryBuilder::by_key("Property", "12345")
        .order_by("ListPrice", "desc")
        .build();

    assert!(result.is_err());
    if let Err(e) = result {
        assert!(matches!(e, ResoError::InvalidQuery(_)));
    }
}

#[test]
fn test_key_access_rejects_apply() {
    let result = QueryBuilder::by_key("Property", "12345")
        .apply("groupby((City))")
        .build();

    assert!(result.is_err());
    if let Err(e) = result {
        assert!(matches!(e, ResoError::InvalidQuery(_)));
    }
}

#[test]
fn test_key_access_rejects_count() {
    let result = QueryBuilder::by_key("Property", "12345")
        .with_count()
        .build();

    assert!(result.is_err());
    if let Err(e) = result {
        assert!(matches!(e, ResoError::InvalidQuery(_)));
    }
}

#[test]
fn test_key_access_rejects_count_only() {
    let result = QueryBuilder::by_key("Property", "12345").count().build();

    assert!(result.is_err());
    if let Err(e) = result {
        assert!(matches!(e, ResoError::InvalidQuery(_)));
    }
}
