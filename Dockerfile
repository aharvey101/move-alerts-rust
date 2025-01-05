# Use the official Rust image as the base image
FROM rust:latest

# Set the working directory inside the container
WORKDIR /usr/src/app

# Copy the source code
COPY . .

# Build the dependencies
RUN cargo build --release 

# Build the application
RUN cargo install --path .

# Set the startup command to run the binary
CMD ["move-alerts-rust"]
