use super::delimited::from_delimited_data;
use super::nuerror::NuIoxErrorHandler;

use super::util::{get_env_var_from_engine, get_runtime, number_of_csv_records};
use nu_engine::CallExt;
use nu_protocol::ast::Call;
use nu_protocol::engine::{Command, EngineState, Stack};
use nu_protocol::{
    Category, Example, PipelineData, ShellError, Signature, Span, Spanned, SyntaxShape, Value,
};

use csv::Trim;

#[derive(Clone)]
pub struct Ioxsql;

impl Command for Ioxsql {
    fn name(&self) -> &str {
        "ioxsql"
    }

    fn signature(&self) -> nu_protocol::Signature {
        Signature::build("ioxsql")
            .required(
                "query",
                SyntaxShape::String,
                "SQL to execute against the database",
            )
            .named(
                "dbname",
                SyntaxShape::String,
                "name of the database to search over",
                Some('d'),
            )
            .category(Category::Filters)
    }

    fn usage(&self) -> &str {
        "Sql query against the Iox Database."
    }

    fn run(
        &self,
        engine_state: &EngineState,
        stack: &mut Stack,
        call: &Call,
        _input: PipelineData,
    ) -> Result<PipelineData, ShellError> {
        let sql: Spanned<String> = call.req(engine_state, stack, 0)?;
        let db: Option<String> = call.get_flag(engine_state, stack, "dbname")?;

        let dbname = if let Some(name) = db {
            name
        } else {
            get_env_var_from_engine(stack, engine_state, "IOX_DBNAME").unwrap()
        };

        let sql_result = tokio_block_sql(&dbname, &sql);
        //println!("sql_result = {:?}", sql_result);

        let numofrecords = number_of_csv_records(&sql_result.as_ref().unwrap());
        //println!("number of csv records = {:?}", numofrecords);

        let not_csv_data = match numofrecords.unwrap() {
            d if d > 0 => false,
            _ => true,
        };

        if not_csv_data {
            let nierrorhandler = NuIoxErrorHandler::new(
                super::nuerror::CommandType::Sql,
                sql_result.as_ref().unwrap().to_string(),
            );

            nierrorhandler.nu_iox_error_check()?;
            nierrorhandler.nu_iox_error_generic(call)?;
        }
        let no_infer = false;
        let noheaders = false;
        let separator: char = ',';
        let trim = Trim::None;

        let input = PipelineData::Value(
            Value::String {
                val: sql_result.unwrap(),
                span: call.head,
            },
            None,
        );

        let name = Span::new(0, 0);
        let config = engine_state.get_config();

        from_delimited_data(noheaders, no_infer, separator, trim, input, name, config)
    }

    fn examples(&self) -> Vec<Example> {
        vec![
            Example {
                description: "Run an sql query against the bananas database",
                example: r#"ioxsql -d bananas "select * from cpu"#,
                result: None,
            },
            Example {
                description: "Run an sql query against the default database",
                example: r#"ioxsql "select * from cpu"#,
                result: None,
            },
        ]
    }
}

pub fn tokio_block_sql(dbname: &String, sql: &Spanned<String>) -> Result<String, std::io::Error> {
    use crate::iox::Nuclient;
    use influxdb_iox_client::connection::Builder;
    let num_threads: Option<usize> = None;
    let tokio_runtime = get_runtime(num_threads)?;

    let sql_result = tokio_runtime.block_on(async move {
        let connection = Builder::default()
            .build("http://127.0.0.1:8082")
            .await
            .expect("client should be valid");

        let mut repl = Nuclient::new(connection);
        repl.use_database(dbname.to_string());
        let _output_format = repl.set_output_format("csv");

        // let rsql = repl.run_sql(sql.item.to_string()).await.expect("run_sql");
        // rsql

        let rsql = repl.run_sql(sql.item.to_string()).await;

        match rsql {
            Ok(res) => res,
            Err(error) => error.to_string(),
        }
    });

    Ok(sql_result)
}
