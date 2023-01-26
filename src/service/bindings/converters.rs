//! Hack to convert from runtime-spec types to bindings types
//!
//! This is only necessary as long as fp-bindgen doesn't provide easy conversion
//! to/from types it imports for the fp-export!-ed top-level types

use fiberplane::{provider_bindings as fp_bind, provider_runtime::spec::types as fp_spec};

/// Trait for fiberplane::provider_runtime::spec types that can convert
/// to fiberplane::provider_bindings types
///
/// The trait is used for convenience (using `.convert()` everywhere a type is wrong)
pub(super) trait SpecToBinding {
    type Target;
    fn convert(self) -> Self::Target;
}

impl SpecToBinding for fp_spec::CheckboxField {
    type Target = fp_bind::CheckboxField;

    fn convert(self) -> Self::Target {
        let mut result = Self::Target::new();
        result.checked = self.checked;
        result.label = self.label;
        result.name = self.name;
        result.required = self.required;
        result.value = self.value;
        result
    }
}

impl SpecToBinding for fp_spec::ConfigField {
    type Target = fp_bind::ConfigField;

    fn convert(self) -> Self::Target {
        match self {
            fp_spec::ConfigField::Checkbox(inner) => Self::Target::Checkbox(inner.convert()),
            fp_spec::ConfigField::Integer(inner) => Self::Target::Integer(inner.convert()),
            fp_spec::ConfigField::Select(inner) => Self::Target::Select(inner.convert()),
            fp_spec::ConfigField::Text(inner) => Self::Target::Text(inner.convert()),
        }
    }
}

impl SpecToBinding for fp_spec::ConfigSchema {
    type Target = fp_bind::ConfigSchema;

    fn convert(self) -> Self::Target {
        self.into_iter().map(|field| field.convert()).collect()
    }
}

impl SpecToBinding for fp_spec::DateTimeRangeField {
    type Target = fp_bind::DateTimeRangeField;

    fn convert(self) -> Self::Target {
        let mut result = Self::Target::new();
        result.name = self.name;
        result.label = self.label;
        result.placeholder = self.placeholder;
        result.required = self.required;
        result
    }
}

impl SpecToBinding for fp_spec::Error {
    type Target = fp_bind::Error;

    fn convert(self) -> Self::Target {
        match self {
            fp_spec::Error::UnsupportedRequest => Self::Target::UnsupportedRequest,
            fp_spec::Error::ValidationError { errors } => Self::Target::ValidationError {
                errors: errors.into_iter().map(|error| error.convert()).collect(),
            },
            fp_spec::Error::Http { error } => Self::Target::Http {
                error: error.convert(),
            },
            fp_spec::Error::Data { message } => Self::Target::Data { message },
            fp_spec::Error::Deserialization { message } => {
                Self::Target::Deserialization { message }
            }
            fp_spec::Error::Config { message } => Self::Target::Config { message },
            fp_spec::Error::NotFound => Self::Target::NotFound,
            fp_spec::Error::ProxyDisconnected => Self::Target::ProxyDisconnected,
            fp_spec::Error::Invocation { message } => Self::Target::Invocation { message },
            fp_spec::Error::Other { message } => Self::Target::Other { message },
        }
    }
}

impl SpecToBinding for fp_spec::FileField {
    type Target = fp_bind::FileField;

    fn convert(self) -> Self::Target {
        let mut result = Self::Target::new();
        result.name = self.name;
        result.label = self.label;
        result.multiple = self.multiple;
        result.required = self.required;
        result
    }
}

impl SpecToBinding for fp_spec::HttpRequest {
    type Target = fp_bind::HttpRequest;

    fn convert(self) -> Self::Target {
        Self::Target {
            url: self.url,
            method: self.method.convert(),
            headers: self.headers,
            body: self.body,
        }
    }
}

impl SpecToBinding for fp_spec::HttpRequestError {
    type Target = fp_bind::HttpRequestError;

    fn convert(self) -> Self::Target {
        match self {
            fp_spec::HttpRequestError::Offline => Self::Target::Offline,
            fp_spec::HttpRequestError::NoRoute => Self::Target::NoRoute,
            fp_spec::HttpRequestError::ConnectionRefused => Self::Target::ConnectionRefused,
            fp_spec::HttpRequestError::Timeout => Self::Target::Timeout,
            fp_spec::HttpRequestError::ResponseTooBig => Self::Target::ResponseTooBig,
            fp_spec::HttpRequestError::ServerError {
                status_code,
                response,
            } => Self::Target::ServerError {
                status_code,
                response,
            },
            fp_spec::HttpRequestError::Other { reason } => Self::Target::Other { reason },
        }
    }
}

impl SpecToBinding for fp_spec::HttpRequestMethod {
    type Target = fp_bind::HttpRequestMethod;

    fn convert(self) -> Self::Target {
        match self {
            fp_spec::HttpRequestMethod::Delete => Self::Target::Delete,
            fp_spec::HttpRequestMethod::Get => Self::Target::Get,
            fp_spec::HttpRequestMethod::Head => Self::Target::Head,
            fp_spec::HttpRequestMethod::Post => Self::Target::Post,
        }
    }
}

impl SpecToBinding for fp_spec::HttpResponse {
    type Target = fp_bind::HttpResponse;

    fn convert(self) -> Self::Target {
        Self::Target {
            body: self.body,
            headers: self.headers,
            status_code: self.status_code,
        }
    }
}

impl SpecToBinding for fp_spec::IntegerField {
    type Target = fp_bind::IntegerField;

    fn convert(self) -> Self::Target {
        let mut result = Self::Target::new();
        result.name = self.name;
        result.label = self.label;
        result.max = self.max;
        result.min = self.min;
        result.placeholder = self.placeholder;
        result.required = self.required;
        result.step = self.step;
        result
    }
}

impl SpecToBinding for fp_spec::LabelField {
    type Target = fp_bind::LabelField;

    fn convert(self) -> Self::Target {
        let mut result = Self::Target::new();
        result.name = self.name;
        result.label = self.label;
        result.multiple = self.multiple;
        result.placeholder = self.placeholder;
        result.required = self.required;
        result
    }
}

impl SpecToBinding for fp_spec::ProviderRequest {
    type Target = fp_bind::ProviderRequest;

    fn convert(self) -> Self::Target {
        Self::Target {
            query_type: self.query_type,
            query_data: self.query_data,
            config: self.config,
            previous_response: self.previous_response,
        }
    }
}

impl SpecToBinding for fp_spec::QueryField {
    type Target = fp_bind::QueryField;

    fn convert(self) -> Self::Target {
        match self {
            fp_spec::QueryField::Checkbox(inner) => Self::Target::Checkbox(inner.convert()),
            fp_spec::QueryField::DateTimeRange(inner) => {
                Self::Target::DateTimeRange(inner.convert())
            }
            fp_spec::QueryField::File(inner) => Self::Target::File(inner.convert()),
            fp_spec::QueryField::Label(inner) => Self::Target::Label(inner.convert()),
            fp_spec::QueryField::Integer(inner) => Self::Target::Integer(inner.convert()),
            fp_spec::QueryField::Select(inner) => Self::Target::Select(inner.convert()),
            fp_spec::QueryField::Text(inner) => Self::Target::Text(inner.convert()),
        }
    }
}

impl SpecToBinding for fp_spec::QuerySchema {
    type Target = fp_bind::QuerySchema;

    fn convert(self) -> Self::Target {
        self.into_iter().map(|field| field.convert()).collect()
    }
}

impl SpecToBinding for fp_spec::SelectField {
    type Target = fp_bind::SelectField;

    fn convert(self) -> Self::Target {
        let mut result = Self::Target::new();
        result.name = self.name;
        result.label = self.label;
        result.multiple = self.multiple;
        result.options = self.options;
        result.placeholder = self.placeholder;
        result.prerequisites = self.prerequisites;
        result.required = self.required;
        result.supports_suggestions = self.supports_suggestions;
        result
    }
}

impl SpecToBinding for fp_spec::SupportedQueryType {
    type Target = fp_bind::SupportedQueryType;

    fn convert(self) -> Self::Target {
        let mut result = Self::Target::new(&self.query_type);
        result.label = self.label;
        result.schema = self.schema.convert();
        result.mime_types = self.mime_types;
        result
    }
}

impl SpecToBinding for fp_spec::TextField {
    type Target = fp_bind::TextField;

    fn convert(self) -> Self::Target {
        let mut result = Self::Target::new();
        result.name = self.name;
        result.label = self.label;
        result.multiline = self.multiline;
        result.multiple = self.multiple;
        result.placeholder = self.placeholder;
        result.prerequisites = self.prerequisites;
        result.required = self.required;
        result.supports_suggestions = self.supports_suggestions;
        result
    }
}

impl SpecToBinding for fp_spec::ValidationError {
    type Target = fp_bind::ValidationError;

    fn convert(self) -> Self::Target {
        todo!()
    }
}
