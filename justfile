# List available commands.
default:
    @just --list

# Build every server.
build:
    cargo build --release --workspace

# Run the bulletin board on 127.0.0.1:8401.
bbs *args:
    cargo run --release -p bbs -- {{args}}

# Run the scheduler on 127.0.0.1:8402.
meet *args:
    cargo run --release -p meet -- {{args}}

# Run the form builder on 127.0.0.1:8403.
forms *args:
    cargo run --release -p forms -- {{args}}

# Run the round-robin tournament maker on 127.0.0.1:8404.
tournament *args:
    cargo run --release -p tournament -- {{args}}

# Run the sign-up sheets on 127.0.0.1:8405.
signup *args:
    cargo run --release -p signup -- {{args}}
