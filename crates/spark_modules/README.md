# spark_modules

Build the module graph from parsed files, collect exports, resolve imports, and diagnose cycles. The [crate entry point](src/lib.rs) describes the inputs and outputs. See [module discovery](src/discovery.rs) for file association rules and [the graph](src/graph.rs) for dependencies.

Use the [multi-file fixtures](../spark_driver/tests/run-pass/multifile) to see layouts and imports that the driver accepts.
