use darling::FromDeriveInput;
use inflector::string::pluralize::to_plural;
use proc_macro2::{Ident, Span};
use syn::{DeriveInput, Path, Result};

/// Values we can parse from #[kube(attrs)]
#[derive(Debug, Default, FromDeriveInput)]
#[darling(attributes(kube))]
pub(crate) struct KubeAttrs {
    group: String,
    version: String,
    kind: String,
    #[darling(default, rename = "struct")]
    kind_struct: Option<String>,
    /// lowercase plural of kind (inferred if omitted)
    #[darling(default)]
    plural: Option<String>,
    #[darling(default)]
    namespaced: bool,
    #[darling(default = "default_apiext")]
    apiextensions: String,
    #[darling(multiple, rename = "derive")]
    derives: Vec<String>,
    #[darling(default)]
    status: Option<String>,
    #[darling(multiple, rename = "shortname")]
    shortnames: Vec<String>,
    #[darling(multiple, rename = "printcolumn")]
    printcolums: Vec<String>,
    #[darling(default)]
    scale: Option<String>,
}

fn default_apiext() -> String {
    "v1".to_owned()
}

pub(crate) fn derive(input: DeriveInput, kube_attrs: KubeAttrs) -> Result<proc_macro2::TokenStream> {
    let KubeAttrs {
        group,
        kind,
        kind_struct,
        version,
        namespaced,
        derives,
        status,
        plural,
        shortnames,
        printcolums,
        apiextensions,
        scale,
    } = kube_attrs;

    let struct_name = kind_struct.unwrap_or_else(|| kind.clone());
    if input.ident == struct_name {
        return Err(syn::Error::new_spanned(
            input.ident,
            r#"#[derive(CustomResource)] `kind = "..."` must not equal the struct name (this is generated)"#,
        ));
    }
    let visibility = input.vis;
    let ident = input.ident;

    // 1. Create root object Foo and truncate name from FooSpec

    // Default visibility is `pub(crate)`
    // Default generics is no generics (makes little sense to re-use CRD kind?)
    // We enforce metadata + spec's existence (always there)
    // => No default impl
    let rootident = Ident::new(&struct_name, Span::call_site());

    // if status set, also add that
    let (statusq, statusdef) = if let Some(status_name) = &status {
        let ident = format_ident!("{}", status_name);
        let fst = quote! {
            #[serde(skip_serializing_if = "Option::is_none")]
            #visibility status: Option<#ident>,
        };
        let snd = quote! { status: None, };
        (fst, snd)
    } else {
        let fst = quote! {};
        let snd = quote! {};
        (fst, snd)
    };
    let has_status = status.is_some();
    let mut has_default = false;

    let mut derive_paths: Vec<Path> = vec![];
    for d in ["::serde::Serialize", "::serde::Deserialize", "Clone", "Debug"].iter() {
        derive_paths.push(syn::parse_str(*d)?);
    }
    for d in &derives {
        if d == "Default" {
            has_default = true; // overridden manually to avoid confusion
        } else {
            derive_paths.push(syn::parse_str(d)?);
        }
    }

    // Schema generation is always enabled for v1 because it's mandatory.
    // TODO Enable schema generation for v1beta1 if the spec derives `JsonSchema`.
    let schema_gen_enabled = apiextensions == "v1" && cfg!(feature = "schema");
    // We exclude fields `apiVersion`, `kind`, and `metadata` from our schema because
    // these are validated by the API server implicitly. Also, we can't generate the
    // schema for `metadata` (`ObjectMeta`) because it doesn't implement `JsonSchema`.
    let schemars_skip = if schema_gen_enabled {
        quote! { #[schemars(skip)] }
    } else {
        quote! {}
    };
    if schema_gen_enabled {
        derive_paths.push(syn::parse_str("::schemars::JsonSchema")?);
    }

    let docstr = format!(" Auto-generated derived type for {} via `CustomResource`", ident);
    let root_obj = quote! {
        #[doc = #docstr]
        #[derive(#(#derive_paths),*)]
        #[serde(rename_all = "camelCase")]
        #visibility struct #rootident {
            #schemars_skip
            #visibility api_version: String,
            #schemars_skip
            #visibility kind: String,
            #schemars_skip
            #visibility metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta,
            #visibility spec: #ident,
            #statusq
        }
        impl #rootident {
            pub fn new(name: &str, spec: #ident) -> Self {
                Self {
                    api_version: <#rootident as k8s_openapi::Resource>::API_VERSION.to_string(),
                    kind: <#rootident as k8s_openapi::Resource>::KIND.to_string(),
                    metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
                        name: Some(name.to_string()),
                        ..Default::default()
                    },
                    spec: spec,
                    #statusdef
                }
            }
        }
    };

    // 2. Implement Resource trait for k8s_openapi
    let api_ver = format!("{}/{}", group, version);
    let impl_resource = quote! {
        impl k8s_openapi::Resource for #rootident {
            const API_VERSION: &'static str = #api_ver;
            const GROUP: &'static str = #group;
            const KIND: &'static str = #kind;
            const VERSION: &'static str = #version;
        }
    };

    // 3. Implement Metadata trait for k8s_openapi
    let impl_metadata = quote! {
        impl k8s_openapi::Metadata for #rootident {
            type Ty = k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
            fn metadata(&self) -> &Self::Ty {
                &self.metadata
            }
            fn metadata_mut(&mut self) -> &mut Self::Ty {
                &mut self.metadata
            }
        }
    };
    // 4. Implement Default if requested
    let impl_default = if has_default {
        quote! {
            impl Default for #rootident {
                fn default() -> Self {
                    Self {
                        api_version: <#rootident as k8s_openapi::Resource>::API_VERSION.to_string(),
                        kind: <#rootident as k8s_openapi::Resource>::KIND.to_string(),
                        metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta::default(),
                        spec: Default::default(),
                        #statusdef
                    }
                }
            }
        }
    } else {
        quote! {}
    };

    // 5. Implement CustomResource
    let name = kind.to_ascii_lowercase();
    let plural = plural.unwrap_or_else(|| to_plural(&name));
    let scope = if namespaced { "Namespaced" } else { "Cluster" };

    // Compute a bunch of crd props
    let mut printers = format!("[ {} ]", printcolums.join(",")); // hacksss
    if apiextensions == "v1beta1" {
        // only major api inconsistency..
        printers = printers.replace("jsonPath", "JSONPath");
    }
    let scale_code = if let Some(s) = scale { s } else { "".to_string() };

    // Ensure it generates for the correct CRD version
    let v1ident = format_ident!("{}", apiextensions);
    let apiext = quote! {
        k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::#v1ident
    };

    let short_json = serde_json::to_string(&shortnames).unwrap();
    let crd_meta_name = format!("{}.{}", plural, group);
    let crd_meta = quote! { { "name": #crd_meta_name } };

    let schemagen = if schema_gen_enabled {
        quote! {
            // Don't use definitions and don't include `$schema` because these are not allowed.
            let gen = schemars::gen::SchemaSettings::openapi3().with(|s| {
                s.inline_subschemas = true;
                s.meta_schema = None;
            }).into_generator();
            let schema = gen.into_root_schema_for::<Self>();
        }
    } else {
        // we could issue a compile time warning for this, but it would hit EVERY compile, which would be noisy
        // eprintln!("warning: kube-derive configured with manual schema generation");
        // users must manually set a valid schema in crd.spec.versions[*].schema - see examples: crd_derive_no_schema
        quote! {
            let schema: Option<k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::JSONSchemaProps> = None;
        }
    };

    let jsondata = if apiextensions == "v1" {
        quote! {
            #schemagen

            let jsondata = serde_json::json!({
                "metadata": #crd_meta,
                "spec": {
                    "group": #group,
                    "scope": #scope,
                    "names": {
                        "plural": #plural,
                        "singular": #name,
                        "kind": #kind,
                        "shortNames": shorts
                    },
                    "versions": [{
                        "name": #version,
                        "served": true,
                        "storage": true,
                        "schema": {
                            "openAPIV3Schema": schema,
                        },
                        "additionalPrinterColumns": columns,
                        "subresources": subres,
                    }],
                }
            });
        }
    } else {
        // TODO Include schema if enabled
        quote! {
            let jsondata = serde_json::json!({
                "metadata": #crd_meta,
                "spec": {
                    "group": #group,
                    "scope": #scope,
                    "names": {
                        "plural": #plural,
                        "singular": #name,
                        "kind": #kind,
                        "shortNames": shorts
                    },
                    // printer columns can't be on versions reliably in v1beta..
                    "additionalPrinterColumns": columns,
                    "versions": [{
                        "name": #version,
                        "served": true,
                        "storage": true,
                    }],
                    "subresources": subres,
                }
            });
        }
    };

    // TODO: should ::crd be from a trait?
    let impl_crd = quote! {
        impl #rootident {
            pub fn crd() -> #apiext::CustomResourceDefinition {
                let columns : Vec<#apiext::CustomResourceColumnDefinition> = serde_json::from_str(#printers).expect("valid printer column json");
                let scale: Option<#apiext::CustomResourceSubresourceScale> = if #scale_code.is_empty() {
                    None
                } else {
                    serde_json::from_str(#scale_code).expect("valid scale subresource json")
                };
                let shorts : Vec<String> = serde_json::from_str(#short_json).expect("valid shortnames");
                let subres = if #has_status {
                    if let Some(s) = &scale {
                        serde_json::json!({
                            "status": {},
                            "scale": scale
                        })
                    } else {
                        serde_json::json!({"status": {} })
                    }
                } else {
                    serde_json::json!({})
                };

                #jsondata
                serde_json::from_value(jsondata)
                    .expect("valid custom resource from #[kube(attrs..)]")
            }
        }
    };

    // Concat output
    Ok(quote! {
        #root_obj
        #impl_resource
        #impl_metadata
        #impl_default
        #impl_crd
    })
}
