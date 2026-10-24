#!/bin/bash
set -euxo pipefail

# Don't wait too long for slow mirrors.
echo 'Acquire::http::Timeout "2";' | tee /etc/apt/apt.conf.d/99timeout
echo 'Acquire::https::Timeout "2";' | tee -a /etc/apt/apt.conf.d/99timeout
echo 'Acquire::Retries "2";' | tee -a /etc/apt/apt.conf.d/99timeout

apt-get update

apt-get install -y --no-install-recommends      \
        cloud-guest-utils

# Expand disk partition
growpart /dev/vda 1

# Expand filesystem
resize2fs /dev/vda1

# Upgrade to the latest trixie kernel; it takes effect from the next boot.
DEBIAN_FRONTEND=noninteractive apt-get install -y linux-image-arm64 linux-headers-arm64

apt-get install -y --no-install-recommends      \
        build-essential                         \
        pkg-config                              \
        libssl-dev                              \
        curl                                    \
        git                                     \
        tmux                                    \
        sysstat                                 \
        ripgrep                                 \
        fish                                    \
        zsh                                     \
        inotify-tools                           \
        avahi-daemon                            \
        socat                                   \
        bat                                     \
        bpftrace                                \
        netcat-openbsd                          \
        neovim                                  \
        wget                                    \
        ca-certificates

# Install docker (from https://docs.docker.com/engine/install/debian/)
# apt update # already present above
# apt install ca-certificates curl # already present above

install -m 0755 -d /etc/apt/keyrings
curl -fsSL https://download.docker.com/linux/debian/gpg -o /etc/apt/keyrings/docker.asc
chmod a+r /etc/apt/keyrings/docker.asc

# Add the repository to Apt sources:
tee /etc/apt/sources.list.d/docker.sources <<EOF
Types: deb
URIs: https://download.docker.com/linux/debian
Suites: $(. /etc/os-release && echo "$VERSION_CODENAME")
Components: stable
Architectures: $(dpkg --print-architecture)
Signed-By: /etc/apt/keyrings/docker.asc
EOF

apt update

apt install -y --no-install-recommends \
    docker-ce \
    docker-ce-cli \
    containerd.io \
    docker-buildx-plugin \
    docker-compose-plugin


# Set hostname to vibe" so it's clear that you're inside the VM.
hostnamectl set-hostname vibe

# Don't display "Last login from ..." on logins
touch .hushlogin

cat > /root/.bashrc <<EOF
# Set this env var so claude doesn't complain about running as root.
export IS_SANDBOX=1

# Set this environment variable to prevent the Gemini CLI from failing to identify the sandbox command
export GEMINI_SANDBOX=false

# Enable true color support in the terminal
export COLORTERM=truecolor

# Hide commands beginning with space from the history
export HISTCONTROL=ignorespace

# Unlimited bash history
export HISTFILESIZE=
export HISTSIZE=

# Use append mode for history
shopt -s histappend
# Write history after every command
PROMPT_COMMAND+=("history -a")

export EDITOR=neovim

export MANPAGER="sh -c 'sed -u -e \"s/\\x1B\[[0-9;]*m//g; s/.\\x08//g\" | bat -p -lman'"
EOF

# Shutdown the VM when you logout
cat > /root/.bash_logout <<EOF

if [[ "/dev/hvc0" == "\$(tty)" ]]; then
  # Sync file system before VM poweroff. Otherwise we may lose recent bash history
  sync --file-system /root
  printf 'a' > /dev/hvc1
  sleep 0.1
elif [[ "/dev/hvc2" == "\$(tty)" ]]; then
  sync --file-system /root
  printf 'b' > /dev/hvc1
  sleep 0.1
elif [[ "/dev/hvc4" == "\$(tty)" ]]; then
  sync --file-system /root
  printf 'c' > /dev/hvc1
  sleep 0.1
elif [[ "/dev/hvc6" == "\$(tty)" ]]; then
  sync --file-system /root
  printf 'd' > /dev/hvc1
  sleep 0.1
fi
EOF

for d in hvc0 hvc2 hvc4 hvc6; do
  mkdir -p /etc/systemd/system/serial-getty@${d}.service.d
  printf '[Service]\nExecStart=\nExecStart=-/sbin/agetty --noclear --autologin root - linux\n' \
> /etc/systemd/system/serial-getty@${d}.service.d/autologin.conf;
done

# Install Rust
curl https://sh.rustup.rs -sSf | sh -s -- -y --profile minimal --component "rustfmt,clippy"
. "$HOME/.cargo/env"

# Install bat (Debian names the binary batcat)
mkdir -p "$HOME/.local/bin"
ln -sf "$(which batcat)" "$HOME/.local/bin/bat"

# Install Mise
curl https://mise.run | sh
echo 'eval "$(~/.local/bin/mise activate bash)"' >> .bashrc

cat > "$HOME/.config/fish/config.fish" <<'FISH'
if status is-interactive
    # Commands to run in interactive sessions can go here
    "$HOME/.local/bin/mise" activate fish | source

    function fish_title
        # If we're connected via ssh, we print the hostname.

        set -l ssh
        set -q SSH_TTY
        and set ssh "["(prompt_hostname | string sub -l 10 | string collect)"]"
        # An override for the current command is passed as the first parameter.
        # This is used by `fg` to show the true process name, among others.
        set ssh "[$VIBE_VM_NAME.local]"
        if set -q argv[1]
            echo -- $ssh (string sub -l 20 -- $argv[1]) (prompt_pwd -d 1 -D 1)
        else
            # Don't print "fish" because it's redundant
            set -l command (status current-command)
            if test "$command" = fish
                set command
            end
            echo -- $ssh (string sub -l 20 -- $command) (prompt_pwd -d 1 -D 1)
        end
    end
end
FISH

export PATH="$HOME/.local/bin:$PATH"
eval "$(mise activate bash)"

mkdir -p .config/mise/

cat > .config/mise/config.toml <<MISE
    [settings]
    # Always use the venv created by uv, if available in directory
    python.uv_venv_auto = true
    idiomatic_version_file_enable_tools = ["rust"]

    # Trust everything by default, since we're already in a VM sandbox
    trusted_config_paths = ["/"]

    [tools]
    uv = { version = "0.13.4" }
    watchexec = { version = "2.5.1" }
MISE

touch .config/mise/mise.lock
mise install

# Done provisioning. vibe powers off the VM after checking that this script succeeded.
