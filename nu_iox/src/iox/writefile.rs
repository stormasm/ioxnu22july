use super::util::{get_env_var_from_engine, get_runtime};
use nu_engine::CallExt;
use nu_protocol::ast::Call;
use nu_protocol::engine::{Command, EngineState, Stack};
use std::fs::File;
use std::io::Read;

use nu_protocol::{Category, Example, PipelineData, ShellError, Signature, SyntaxShape, Value};

#[derive(Clone)]
pub struct Ioxwritefile;

impl Command for Ioxwritefile {
    fn name(&self) -> &str {
        "ioxwritefile"
    }

    fn signature(&self) -> nu_protocol::Signature {
        Signature::build("ioxwritefile")
            .required(
                "filename",
                SyntaxShape::String,
                "File name that contains influxdb line protocol data",
            )
            .named(
                "dbname",
                SyntaxShape::String,
                "name of the database to write to",
                Some('d'),
            )
            .category(Category::Filters)
    }

    fn usage(&self) -> &str {
        "Write a line protocol file to the Iox Database."
    }

    fn run(
        &self,
        engine_state: &EngineState,
        stack: &mut Stack,
        call: &Call,
        _input: PipelineData,
    ) -> Result<PipelineData, ShellError> {
        let filename: String = call.req(engine_state, stack, 0)?;
        let db: Option<String> = call.get_flag(engine_state, stack, "dbname")?;

        let dbname = if let Some(name) = db {
            name
        } else {
            get_env_var_from_engine(stack, engine_state, "IOX_DBNAME").unwrap()
        };

        println!("dbname = {:?}", dbname);

        //let mut file = File::open(filename).unwrap();
        let mut file = File::open(filename)
            .map_err(|e| ShellError::ReadingFile(e.to_string(), call.span()))?;

        let mut lp_data = String::new();
        let _ = file.read_to_string(&mut lp_data);

        let nol_result = tokio_block_writefile(&dbname, &lp_data);

        println!("{:?}", nol_result);

        Ok(PipelineData::Value(
            Value::Nothing { span: call.head },
            None,
        ))
    }

    fn examples(&self) -> Vec<Example> {
        vec![
            Example {
                description: "Write a line protocol data file out to Iox",
                example: r#"ioxwritefile ./test_fixtures/lineproto/temperature.lp"#,
                result: None,
            },
            Example {
                description: "Write a line protocol data file out to Iox",
                example: r#"ioxwritefile ./ioxnotes/lineproto/popnm.lp"#,
                result: None,
            },
        ]
    }
}

pub fn tokio_block_writefile(dbname: &String, lp_data: &String) -> Result<String, std::io::Error> {
    use influxdb_iox_client::{connection::Builder, write::Client};

    let num_threads: Option<usize> = None;
    let tokio_runtime = get_runtime(num_threads)?;

    let nol_result = tokio_runtime.block_on(async move {
        let connection = Builder::default()
            .build("http://127.0.0.1:8081")
            .await
            .expect("client should be valid");

        let mut client = Client::new(connection);

        let numoflines = client
            .write_lp(dbname.to_string(), lp_data.to_string(), 0)
            .await;

        match numoflines {
            Ok(res) => res.to_string(),
            Err(error) => error.to_string(),
        }
    });

    Ok(nol_result)
}
