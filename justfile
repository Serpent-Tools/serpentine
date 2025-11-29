run entry_point="DEFAULT":
    cargo run -- run --entry-point {{entry_point}}

graph:
    cargo run -- graph

clean:
    cargo run -- clean
