{
  description = "Agent Shell — Linux desktop agent shell";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        rustPlatform = pkgs.rustPlatform;

        # §20.6 打包依赖清单：系统依赖
        buildInputs = with pkgs; [
          pipewire
          systemdMinimal
          pkg-config
          # extension.js 的 GJS 语法校验（doCheck 跑 `gjs --check-syntax`）
          gjs
        ];

        # agent-shell workspace 根
        workspaceRoot = ./../..;

        # §20.2 打包：agent-shell（daemon + cli + mcp）
        agent-shell = rustPlatform.buildRustPackage {
          pname = "agent-shell";
          version = "0.1.0";
          src = workspaceRoot;

          cargoLock = {
            lockFile = "${workspaceRoot}/Cargo.lock";
          };

          inherit buildInputs;

          # D2 编译策略：release profile lto + strip
          # （Cargo.toml [profile.release] 已定义，cargo build --release 自动应用）

          # 只构建 daemon + cli + mcp 二进制
          buildPhase = ''
            runHook preBuild
            cargo build --release -p agent-shell-daemon -p agent-shell-cli -p mcp
            runHook postBuild
          '';

          # 安装 systemd user service + udev rules + 二进制
          postInstall = ''
            # systemd user service（§22.2 D1）
            install -Dm644 daemon/agent-shell-daemon.service \
              $out/lib/systemd/user/agent-shell-daemon.service
            # udev rules for ydotool /dev/uinput（§20.3 权限模型）
            install -Dm644 packaging/debian/60-agent-shell-uinput.rules \
              $out/lib/udev/rules.d/60-agent-shell-uinput.rules
            # GNOME Shell Extension（§8.1 系统级部署；用户级启用走 `agent-shell extension enable`）
            install -Dm644 components/compositor/mutter/src/extension.js \
              $out/share/gnome-shell/extensions/agent-shell-bridge@tsic.top/extension.js
            install -Dm644 components/compositor/mutter/src/metadata.json \
              $out/share/gnome-shell/extensions/agent-shell-bridge@tsic.top/metadata.json
          '';

          doCheck = true;

          meta = with pkgs.lib; {
            description = "Agent Shell — Linux desktop agent shell (daemon + CLI + MCP)";
            license = licenses.mit;
            platforms = platforms.linux;
          };
        };

        # §20.2 打包：agent-shell-rootd（特权代理，独立可装）
        agent-shell-rootd = rustPlatform.buildRustPackage {
          pname = "agent-shell-rootd";
          version = "0.1.0";
          src = workspaceRoot;

          cargoLock = {
            lockFile = "${workspaceRoot}/Cargo.lock";
          };

          buildInputs = with pkgs; [
            systemdMinimal
            pkg-config
          ];

          buildPhase = ''
            runHook preBuild
            cargo build --release -p agent-shell-rootd
            runHook postBuild
          '';

          # rootd 二进制 + polkit policy + pkexec + systemd system unit
          postInstall = ''
            # polkit policy（§23.4.2）
            install -Dm644 packaging/com.agentshell.policy \
              $out/share/polkit-1/actions/com.agentshell.policy
            # pkexec 兜底入口（§23.4.1 通道 3）
            install -Dm755 packaging/rootd-pkexec \
              $out/lib/agent-shell/rootd-pkexec
            # D-Bus system policy（允许 rootd 注册 org.agentshell.Rootd）
            install -Dm644 packaging/org.agentshell.Rootd.conf \
              $out/share/dbus-1/system.d/org.agentshell.Rootd.conf
            install -Dm644 rootd/agent-shell-rootd.service \
              $out/lib/systemd/system/agent-shell-rootd.service
          '';

          doCheck = true;

          meta = with pkgs.lib; {
            description = "Agent Shell rootd — privileged proxy (systemd system unit, polkit whitelist)";
            license = licenses.mit;
            platforms = platforms.linux;
          };
        };
      in
      {
        packages = {
          inherit agent-shell agent-shell-rootd;
          default = agent-shell;
        };

        devShells.default = pkgs.mkShell {
          inherit buildInputs;
        };
      });
}
