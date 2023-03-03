use super::delimited::from_delimited_data;
use super::util::get_runtime;
use nu_protocol::ast::Call;
use nu_protocol::engine::{Command, EngineState, Stack};
use nu_protocol::{Category, Example, PipelineData, ShellError, Signature, Span, Value};

use csv::Trim;

#[derive(Clone)]
pub struct Ioxnamespace;

impl Command for Ioxnamespace {
    fn name(&self) -> &str {
        "ioxnamespace"
    }

    fn signature(&self) -> nu_protocol::Signature {
        Signature::build("ioxnamespace").category(Category::Filters)
    }

    fn usage(&self) -> &str {
        "Show all of the Iox Databases."
    }

    fn run(
        &self,
        engine_state: &EngineState,
        _stack: &mut Stack,
        call: &Call,
        _input: PipelineData,
    ) -> Result<PipelineData, ShellError> {
        let namespace_result = tokio_block_namespace();

        let no_infer = false;
        let noheaders = false;
        let separator: char = ',';
        let trim = Trim::None;

        let input = PipelineData::Value(
            Value::String {
                val: namespace_result.unwrap(),
                span: call.head,
            },
            None,
        );

        let name = Span::new(0, 0);
        let config = engine_state.get_config();

        from_delimited_data(noheaders, no_infer, separator, trim, input, name, config)
    }

    fn examples(&self) -> Vec<Example> {
        vec![Example {
            description: "Show the databases or namespaces",
            example: r#"ioxnamespace"#,
            result: None,
        }]
    }
}

pub fn tokio_block_namespace() -> Result<String, std::io::Error> {
    use crate::iox::Nuclient;
    use influxdb_iox_client::connection::Builder;
    let num_threads: Option<usize> = None;
    let tokio_runtime = get_runtime(num_threads)?;

    let namespace = tokio_runtime.block_on(async move {
        let connection = Builder::default()
            .build("http://127.0.0.1:8082")
            .await
            .expect("client should be valid");

        let mut repl = Nuclient::new(connection);
        let _output_format = repl.set_output_format("csv");

        let namespace = repl.list_namespaces().await.expect("namespaces");
        namespace
    });

    Ok(namespace)
}
