use super::{
    flags::*, flow_control::{insert_statement, Case, ElseIf, Function, Statement},
    job_control::JobControl, status::*, Shell,
};
use parser::{
    assignments::is_array, expand_string, parse_and_validate, pipelines::Pipeline, ForExpression,
    StatementSplitter,
};
use shell::{assignments::VariableStore, variables::VariableType};
use small;
use std::{
    io::{stdout, Write}, iter,
};
use types;

#[derive(Debug)]
pub(crate) enum Condition {
    Continue,
    Break,
    NoOp,
    SigInt,
}

pub(crate) trait FlowLogic {
    /// Receives a command and attempts to execute the contents.
    fn on_command(&mut self, command_string: &str);

    /// Executes all of the statements within a while block until a certain
    /// condition is met.
    fn execute_while(&mut self, expression: Pipeline, statements: Vec<Statement>) -> Condition;

    /// Executes all of the statements within a for block for each value
    /// specified in the range.
    fn execute_for(
        &mut self,
        variable: &str,
        values: &[small::String],
        statements: Vec<Statement>,
    ) -> Condition;

    /// Conditionally executes branches of statements according to evaluated
    /// expressions
    fn execute_if(
        &mut self,
        expression: Box<Statement>,
        success: Vec<Statement>,
        else_if: Vec<ElseIf>,
        failure: Vec<Statement>,
    ) -> Condition;

    /// Simply executes all supplied statements.
    fn execute_statements(&mut self, statements: Vec<Statement>) -> Condition;

    /// Executes a single statement
    fn execute_statement(&mut self, statement: Statement) -> Condition;

    /// Expand an expression and run a branch based on the value of the
    /// expanded expression
    fn execute_match(&mut self, expression: small::String, cases: Vec<Case>) -> Condition;
}

impl FlowLogic for Shell {
    fn execute_if(
        &mut self,
        expression: Box<Statement>,
        success: Vec<Statement>,
        else_if: Vec<ElseIf>,
        failure: Vec<Statement>,
    ) -> Condition {
        let first_condition = iter::once((expression, success));
        let else_conditions = else_if
            .into_iter()
            .map(|cond| (cond.expression, cond.success));

        for (condition, statements) in first_condition.chain(else_conditions) {
            if let Condition::SigInt = self.execute_statements(vec![*condition]) {
                return Condition::SigInt;
            }

            if self.previous_status == 0 {
                return self.execute_statements(statements);
            }
        }

        self.execute_statements(failure)
    }

    fn execute_for(
        &mut self,
        variable: &str,
        values: &[small::String],
        statements: Vec<Statement>,
    ) -> Condition {
        let ignore_variable = variable == "_";
        match ForExpression::new(values, self) {
            ForExpression::Multiple(ref values) if ignore_variable => for _ in values.iter() {
                match self.execute_statements(statements.clone()) {
                    Condition::Break => break,
                    Condition::SigInt => return Condition::SigInt,
                    _ => (),
                }
            },
            ForExpression::Multiple(values) => for value in &values {
                self.set(variable, value.clone());
                match self.execute_statements(statements.clone()) {
                    Condition::Break => break,
                    Condition::SigInt => return Condition::SigInt,
                    _ => (),
                }
            },
            ForExpression::Normal(ref values) if ignore_variable => for _ in values.lines() {
                match self.execute_statements(statements.clone()) {
                    Condition::Break => break,
                    Condition::SigInt => return Condition::SigInt,
                    _ => (),
                }
            },
            ForExpression::Normal(values) => for value in values.lines() {
                self.set(variable, value);
                match self.execute_statements(statements.clone()) {
                    Condition::Break => break,
                    Condition::SigInt => return Condition::SigInt,
                    _ => (),
                }
            },
            ForExpression::Range(start, end) if ignore_variable => for _ in start..end {
                match self.execute_statements(statements.clone()) {
                    Condition::Break => break,
                    Condition::SigInt => return Condition::SigInt,
                    _ => (),
                }
            },
            ForExpression::Range(start, end) => for value in (start..end).map(|x| x.to_string()) {
                self.set(variable, value.clone());
                match self.execute_statements(statements.clone()) {
                    Condition::Break => break,
                    Condition::SigInt => return Condition::SigInt,
                    _ => (),
                }
            },
        }
        Condition::NoOp
    }

    fn execute_while(&mut self, expression: Pipeline, statements: Vec<Statement>) -> Condition {
        while self.run_pipeline(&mut expression.clone()) == Some(SUCCESS) {
            // Cloning is needed so the statement can be re-iterated again if needed.
            match self.execute_statements(statements.clone()) {
                Condition::Break => break,
                Condition::SigInt => return Condition::SigInt,
                _ => (),
            }
        }
        Condition::NoOp
    }

    fn execute_statement(&mut self, statement: Statement) -> Condition {
        match statement {
            Statement::Error(number) => self.previous_status = number,
            Statement::Let(action) => {
                self.previous_status = self.local(action);
                self.variables.set("?", self.previous_status.to_string());
            }
            Statement::Export(action) => {
                self.previous_status = self.export(action);
                self.variables.set("?", self.previous_status.to_string());
            }
            Statement::While {
                expression,
                statements,
            } => {
                if let Condition::SigInt = self.execute_while(expression, statements) {
                    return Condition::SigInt;
                }
            }
            Statement::For {
                variable,
                values,
                statements,
            } => {
                if let Condition::SigInt = self.execute_for(&variable, &values, statements) {
                    return Condition::SigInt;
                }
            }
            Statement::If {
                expression,
                success,
                else_if,
                failure,
                ..
            } => match self.execute_if(expression, success, else_if, failure) {
                Condition::Break => return Condition::Break,
                Condition::Continue => return Condition::Continue,
                Condition::NoOp => (),
                Condition::SigInt => return Condition::SigInt,
            },
            Statement::Function {
                name,
                args,
                statements,
                description,
            } => {
                self.variables.set(
                    &name,
                    Function::new(description, name.clone(), args, statements),
                );
            }
            Statement::Pipeline(mut pipeline) => {
                self.run_pipeline(&mut pipeline);
                if self.flags & ERR_EXIT != 0 && self.previous_status != SUCCESS {
                    let status = self.previous_status;
                    self.exit(status);
                }
            }
            Statement::Time(box_statement) => {
                let time = ::std::time::Instant::now();

                let condition = self.execute_statement(*box_statement);

                let duration = time.elapsed();
                let seconds = duration.as_secs();
                let nanoseconds = duration.subsec_nanos();

                let stdout = stdout();
                let mut stdout = stdout.lock();
                let _ = if seconds > 60 {
                    writeln!(
                        stdout,
                        "real    {}m{:02}.{:09}s",
                        seconds / 60,
                        seconds % 60,
                        nanoseconds
                    )
                } else {
                    writeln!(stdout, "real    {}.{:09}s", seconds, nanoseconds)
                };
                match condition {
                    Condition::Break => return Condition::Break,
                    Condition::Continue => return Condition::Continue,
                    Condition::NoOp => (),
                    Condition::SigInt => return Condition::SigInt,
                }
            }
            Statement::And(box_statement) => {
                let condition = match self.previous_status {
                    SUCCESS => self.execute_statement(*box_statement),
                    _ => Condition::NoOp,
                };

                match condition {
                    Condition::Break => return Condition::Break,
                    Condition::Continue => return Condition::Continue,
                    Condition::NoOp => (),
                    Condition::SigInt => return Condition::SigInt,
                }
            }
            Statement::Or(box_statement) => {
                let condition = match self.previous_status {
                    FAILURE => self.execute_statement(*box_statement),
                    _ => Condition::NoOp,
                };

                match condition {
                    Condition::Break => return Condition::Break,
                    Condition::Continue => return Condition::Continue,
                    Condition::NoOp => (),
                    Condition::SigInt => return Condition::SigInt,
                }
            }
            Statement::Not(box_statement) => {
                // NOTE: Should the condition be used?
                let _condition = self.execute_statement(*box_statement);
                match self.previous_status {
                    FAILURE => self.previous_status = SUCCESS,
                    SUCCESS => self.previous_status = FAILURE,
                    _ => (),
                }
                let previous_status = self.previous_status.to_string();
                self.set("?", previous_status);
            }
            Statement::Break => return Condition::Break,
            Statement::Continue => return Condition::Continue,
            Statement::Match { expression, cases } => match self.execute_match(expression, cases) {
                Condition::Break => return Condition::Break,
                Condition::Continue => return Condition::Continue,
                Condition::NoOp => (),
                Condition::SigInt => return Condition::SigInt,
            },
            _ => {}
        }
        if let Some(signal) = self.next_signal() {
            if self.handle_signal(signal) {
                self.exit(get_signal_code(signal));
            }
            Condition::SigInt
        } else if self.break_flow {
            self.break_flow = false;
            Condition::SigInt
        } else {
            Condition::NoOp
        }
    }

    fn execute_statements(&mut self, statements: Vec<Statement>) -> Condition {
        self.variables.new_scope(false);

        let mut condition = None;
        for statement in statements {
            match self.execute_statement(statement) {
                Condition::NoOp => {}
                cond => {
                    condition = Some(cond);
                    break;
                }
            }
        }

        self.variables.pop_scope();

        condition.unwrap_or(Condition::NoOp)
    }

    fn execute_match(&mut self, expression: small::String, cases: Vec<Case>) -> Condition {
        // Logic for determining if the LHS of a match-case construct (the value we are
        // matching against) matches the RHS of a match-case construct (a value
        // in a case statement). For example, checking to see if the value
        // "foo" matches the pattern "bar" would be invoked like so :
        // ```ignore
        // matches("foo", "bar") 
        // ```
        fn matches(lhs: &types::Array, rhs: &types::Array) -> bool {
            for v in lhs {
                if rhs.contains(&v) {
                    return true;
                }
            }
            false
        }

        let is_array = is_array(&expression);
        let value = expand_string(&expression, self, false);
        let mut condition = Condition::NoOp;
        for case in cases {
            // let pattern_is_array = is_array(&value);
            let pattern = case.value.map(|v| expand_string(&v, self, false));
            match pattern {
                None => {
                    let mut previous_bind = None;
                    if let Some(ref bind) = case.binding {
                        if is_array {
                            previous_bind = self
                                .variables
                                .get::<types::Array>(bind)
                                .map(|x| VariableType::Array(x));
                            self.variables.set(&bind, value.clone());
                        } else {
                            previous_bind = self
                                .variables
                                .get::<types::Str>(bind)
                                .map(|x| VariableType::Str(x));
                            self.set(&bind, value.join(" "));
                        }
                    }

                    if let Some(statement) = case.conditional {
                        self.on_command(&statement);
                        if self.previous_status != SUCCESS {
                            continue;
                        }
                    }

                    condition = self.execute_statements(case.statements);

                    if let Some(ref bind) = case.binding {
                        if let Some(value) = previous_bind {
                            match value {
                                str_ @ VariableType::Str(_) => {
                                    self.set(bind, str_);
                                }
                                array @ VariableType::Array(_) => {
                                    self.variables.set(bind, array);
                                }
                                map @ VariableType::HashMap(_) => {
                                    self.variables.set(bind, map);
                                }
                                _ => (),
                            }
                        }
                    }

                    break;
                }
                Some(ref v) if matches(v, &value) => {
                    let mut previous_bind = None;
                    if let Some(ref bind) = case.binding {
                        if is_array {
                            previous_bind = self
                                .variables
                                .get::<types::Array>(bind)
                                .map(|x| VariableType::Array(x));
                            self.variables.set(&bind, value.clone());
                        } else {
                            previous_bind = self
                                .variables
                                .get::<types::Str>(bind)
                                .map(|x| VariableType::Str(x));
                            self.set(&bind, value.join(" "));
                        }
                    }

                    if let Some(statement) = case.conditional {
                        self.on_command(&statement);
                        if self.previous_status != SUCCESS {
                            continue;
                        }
                    }

                    condition = self.execute_statements(case.statements);

                    if let Some(ref bind) = case.binding {
                        if let Some(value) = previous_bind {
                            match value {
                                str_ @ VariableType::Str(_) => {
                                    self.set(bind, str_);
                                }
                                array @ VariableType::Array(_) => {
                                    self.set(bind, array);
                                }
                                map @ VariableType::HashMap(_) => {
                                    self.set(bind, map);
                                }
                                _ => (),
                            }
                        }
                    }

                    break;
                }
                Some(_) => (),
            }
        }
        condition
    }

    fn on_command(&mut self, command_string: &str) {
        self.break_flow = false;
        let iterator = StatementSplitter::new(command_string).map(parse_and_validate);

        // Go through all of the statements and build up the block stack
        // When block is done return statement for execution.
        for statement in iterator {
            match insert_statement(&mut self.flow_control, statement) {
                Err(why) => {
                    eprintln!("{}", why);
                    self.flow_control.reset();
                    return;
                }
                Ok(Some(stm)) => {
                    let _ = self.execute_statement(stm);
                }
                Ok(None) => {}
            }
        }
    }
}
