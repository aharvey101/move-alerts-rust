# Use the official Rust image as the base image
FROM rust:latest

# Set the working directory inside the container
WORKDIR /usr/src/app

# Copy the Cargo.toml and Cargo.lock files
COPY Cargo.toml Cargo.lock ./

# Build the dependencies
RUN cargo build --release --target x86_64-unknown-linux-gnu

# Copy the source code
COPY . .

# Build the application
RUN cargo install --path .

# Set the startup command to run the binary
CMD ["move-alerts-rust"]
