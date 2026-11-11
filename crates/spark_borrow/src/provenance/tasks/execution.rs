// TAG [SPARK-114]

//! Function-typed backing for the current callback of a task execution.

use spark_mir::Local;
use spark_types::TypeId;

use crate::provenance::{State, Value};

impl State {
    pub(in crate::provenance) fn clobber_execution_callback(
        &mut self,
        local: Local,
        ty: TypeId,
        callback: Value,
    ) {
        self.input_owners.escape_local(local);
        self.future_owners.invalidate(local, &self.future_copies);
        self.invalidated_copies.insert(local);
        self.executions.insert((local, ty), callback);
    }
}
