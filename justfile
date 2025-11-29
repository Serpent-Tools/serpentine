run entry_point="DEFAULT":
    cargo run -- run --entry-point {{entry_point}}

graph:
    cargo run -- graph


# A failing test from serpentine might leave containers running
# To be clear we mean a failing test in serpentine's test suite,
# serpentine itself will only leave containers hanging in the event of a panic or other unexpected pre-mature shutdown
# (ctrl-c / SIGINT is not considered a pre-mature shutdown, and does cause cleanup paths to run).
# 
# WARNING: This might stop/delete other images on your system
# Its only recommended to be run if you don't have other running important containers
clean:
    cargo run -p serpentine -- clean
    cargo clean
    docker stop --all
    docker rm --all
    docker system prune --all
