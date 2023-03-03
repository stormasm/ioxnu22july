use super::util::{get_env_var_from_engine, get_runtime};
use nu_engine::CallExt;
use nu_protocol::ast::Call;
use nu_protocol::engine::{Command, EngineState, Stack};

use nu_protocol::{
    Category, Example, PipelineData, ShellError, Signature, Spanned, SyntaxShape, Value,
};

#[derive(Clone)]
pub struct Ioxwrite;

impl Command for Ioxwrite {
    fn name(&self) -> &str {
        "ioxwrite"
    }

    fn signature(&self) -> nu_protocol::Signature {
        Signature::build("ioxwrite")
            .required(
                "data",
                SyntaxShape::String,
                "Line protocol string to write to Iox",
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
        "Write data to the Iox Database."
    }

    fn run(
        &self,
        engine_state: &EngineState,
        stack: &mut Stack,
        call: &Call,
        _input: PipelineData,
    ) -> Result<PipelineData, ShellError> {
        let lp_data: Spanned<String> = call.req(engine_state, stack, 0)?;
        let db: Option<String> = call.get_flag(engine_state, stack, "dbname")?;

        let dbname = if let Some(name) = db {
            name
        } else {
            get_env_var_from_engine(stack, engine_state, "IOX_DBNAME").unwrap()
        };

        println!("dbname = {:?}", dbname);

        let nol_result = tokio_block_write(&dbname, &lp_data);

        println!("{:?}", nol_result);

        Ok(PipelineData::Value(
            Value::Nothing { span: call.head },
            None,
        ))
    }

    fn examples(&self) -> Vec<Example> {
        vec![
            Example {
                description: "Write some line protocol data out to Iox using the bananas db",
                example: r#"ioxwrite -d bananas "cpu,region=la user=955111599 222522""#,
                result: None,
            },
            Example {
                description: "Write some line protocol data out to Iox using the default db",
                example: r#"ioxwrite "cpu,region=pa user=9599 222522""#,
                result: None,
            },
            Example {
                description: "Write some line protocol data out to Iox using the default db",
                example: r#"ioxwrite "pop_nm,location=corvallis,state=OR population=51023,zipcode=97333 1700756160""#,
                result: None,
            },
            Example {
                description: "Write some line protocol data out to Iox using the default db",
                example: r#"ioxwrite "cpu,region=pa user=9599 222522""#,
                result: None,
            },
        ]
    }
}

pub fn tokio_block_write(
    dbname: &String,
    lp_data: &Spanned<String>,
) -> Result<String, std::io::Error> {
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
            .write_lp(dbname.to_string(), lp_data.item.to_string(), 0)
            .await;

        match numoflines {
            Ok(res) => res.to_string(),
            Err(error) => error.to_string(),
        }
    });

    Ok(nol_result)
}
