# Dockerfile for creating Windows NSIS installers from macOS/Linux
# NSIS runs natively on Linux - no Wine needed!

FROM ubuntu:24.04

# Prevent interactive prompts during installation
ENV DEBIAN_FRONTEND=noninteractive

# Install NSIS and tools (NSIS runs natively, creates Windows installers)
RUN apt-get update && \
    apt-get install -y \
        nsis \
        nsis-pluginapi \
        osslsigncode \
        curl \
        unzip \
        p7zip-full \
        perl \
        imagemagick \
        && rm -rf /var/lib/apt/lists/*

# Create working directory
WORKDIR /workspace

# Copy the packaging script (NSIS script is generated inline)
COPY docker/package-windows-inside-docker.sh /workspace/package.sh
RUN chmod +x /workspace/package.sh

CMD ["/workspace/package.sh"]
