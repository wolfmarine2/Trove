import type { Repository } from "./api";

export function formatBytes(n: number): string {
  if (!Number.isFinite(n) || n <= 0) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB"];
  const i = Math.min(units.length - 1, Math.floor(Math.log(n) / Math.log(1024)));
  const v = n / 1024 ** i;
  return `${v >= 100 || i === 0 ? v.toFixed(0) : v.toFixed(1)} ${units[i]}`;
}

export function formatTime(ms: number | null | undefined, lang: string): string {
  if (!ms) return "—";
  return new Date(ms).toLocaleString(lang === "zh" ? "zh-CN" : "en-GB", {
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  });
}

export function relativeTime(ms: number | null | undefined, lang: string): string {
  if (!ms) return "—";
  const rtf = new Intl.RelativeTimeFormat(lang === "zh" ? "zh-CN" : "en", { numeric: "auto" });
  const diff = (ms - Date.now()) / 1000;
  const abs = Math.abs(diff);
  if (abs < 60) return rtf.format(Math.round(diff), "second");
  if (abs < 3600) return rtf.format(Math.round(diff / 60), "minute");
  if (abs < 86400) return rtf.format(Math.round(diff / 3600), "hour");
  if (abs < 86400 * 30) return rtf.format(Math.round(diff / 86400), "day");
  return formatTime(ms, lang);
}

/** Address users type: Docker images live under host/repo, others under the repository URL. */
export function displayUrl(repo: Repository): string {
  if (repo.format === "docker") return `${repo.url.replace(/^https?:\/\//, "").replace("/repository/", "/")}/`;
  return `${repo.url}/`;
}

function suiteHint(remote: string | undefined): string {
  if (!remote) return "<suite>";
  if (remote.includes("ubuntu")) return "noble";
  if (remote.includes("debian")) return "bookworm";
  return "<suite>";
}

export interface Snippet {
  file: string;
  code: string;
}

/**
 * Client configuration for a repository, generated from its format and type.
 * Reads need credentials only when anonymous read is switched off; publishing
 * to hosted repositories always does.
 */
export function clientSnippets(repo: Repository, anonymousRead: boolean): Snippet[] {
  const url = repo.url;
  const host = url.replace(/^https?:\/\//, "");
  const canPublish = repo.kind === "hosted";
  const readAuth = !anonymousRead || !!repo.config.private;
  switch (repo.format) {
    case "maven": {
      const servers = readAuth || canPublish;
      const out: Snippet[] = [
        {
          file: "~/.m2/settings.xml",
          code: `<settings>${
            servers
              ? `
  <servers>
    <server>
      <id>${repo.name}</id>
      <username>your-user</username>
      <password><TOKEN></password>
    </server>
  </servers>`
              : ""
          }
  <mirrors>
    <mirror>
      <id>${repo.name}</id>
      <mirrorOf>*</mirrorOf>
      <url>${url}/</url>
    </mirror>
  </mirrors>
</settings>`,
        },
      ];
      if (canPublish) {
        const tag = repo.config.version_policy === "snapshot" ? "snapshotRepository" : "repository";
        out.push({
          file: "pom.xml",
          code: `<distributionManagement>
  <${tag}>
    <id>${repo.name}</id>
    <url>${url}/</url>
  </${tag}>
</distributionManagement>`,
        });
      }
      out.push({
        file: "build.gradle.kts",
        code: readAuth
          ? `repositories {
    maven {
        url = uri("${url}/")
        credentials { username = "your-user"; password = "<TOKEN>" }
    }
}`
          : `repositories {
    maven { url = uri("${url}/") }
}`,
      });
      return out;
    }
    case "npm":
      if (canPublish)
        return [
          { file: ".npmrc", code: `@your-scope:registry=${url}/\n//${host}/:_authToken=<TOKEN>` },
          { file: "shell", code: `npm publish --registry ${url}/` },
        ];
      return [
        { file: ".npmrc", code: readAuth ? `registry=${url}/\n//${host}/:_authToken=<TOKEN>` : `registry=${url}/` },
        { file: "shell", code: `npm config set registry ${url}/\npnpm config set registry ${url}/\nyarn config set npmRegistryServer ${url}/` },
      ];
    case "pypi": {
      const index = readAuth
        ? `${url.replace("://", "://your-user:<TOKEN>@")}/simple/`
        : `${url}/simple/`;
      const install: Snippet[] = [
        { file: "pip.conf", code: `[global]\nindex-url = ${index}${url.startsWith("http://") ? `\ntrusted-host = ${host.split("/")[0]}` : ""}` },
        { file: "shell", code: `pip install --index-url ${index} requests\nuv pip install --index-url ${index} requests` },
      ];
      if (!canPublish) return install;
      return [
        {
          file: "~/.pypirc",
          code: `[distutils]
index-servers = ${repo.name}

[${repo.name}]
repository = ${url}/
username = __token__
password = <TOKEN>`,
        },
        { file: "shell", code: `twine upload --repository ${repo.name} dist/*` },
        ...install,
      ];
    }
    case "cargo": {
      const index = `sparse+${url}/index/`;
      const out: Snippet[] = [
        {
          file: ".cargo/config.toml",
          code:
            repo.kind === "hosted"
              ? `[registries.${repo.name}]
index = "${index}"`
              : `[registries.${repo.name}]
index = "${index}"

# Send every crates.io download through Trove
[source.crates-io]
replace-with = "${repo.name}"

[source.${repo.name}]
registry = "${index}"`,
        },
      ];
      if (canPublish || readAuth) out.push({ file: "shell", code: `cargo login --registry ${repo.name} <TOKEN>` });
      if (canPublish) out.push({ file: "shell", code: `cargo publish --registry ${repo.name}` });
      return out;
    }
    case "docker": {
      const origin = url.replace(/\/repository\/.*$/, "");
      const registry = origin.replace(/^https?:\/\//, "");
      const isHub = (repo.config.proxy?.remote_url ?? "").includes("docker.io") || repo.kind === "group";
      const image = isHub ? "library/nginx:1.27" : "<image>:<tag>";
      return [
        { file: "shell", code: `${readAuth ? `docker login ${registry}\n` : ""}docker pull ${registry}/${repo.name}/${image}` },
        {
          file: "/etc/containerd/certs.d/docker.io/hosts.toml",
          code: `server = "https://registry-1.docker.io"

[host."${origin}/v2/${repo.name}"]
  capabilities = ["pull", "resolve"]
  override_path = true`,
        },
        {
          file: "/etc/docker/daemon.json",
          code: `{
  "registry-mirrors": ["${origin}"]${origin.startsWith("http://") ? `,\n  "insecure-registries": ["${registry}"]` : ""}
}`,
        },
        { file: "deployment.yaml", code: `image: ${registry}/${repo.name}/${image}` },
      ];
    }
    case "apt": {
      const base = readAuth ? `${url.replace("://", "://your-user:<TOKEN>@")}` : url;
      return [
        { file: "/etc/apt/sources.list.d/trove.list", code: `deb ${base}/ ${suiteHint(repo.config.proxy?.remote_url)} main` },
        {
          file: "/etc/apt/sources.list.d/trove.sources",
          code: `Types: deb
URIs: ${base}/
Suites: ${suiteHint(repo.config.proxy?.remote_url)}
Components: main
Signed-By: /usr/share/keyrings/<distribution>-archive-keyring.gpg`,
        },
      ];
    }
    case "yum":
      return [
        {
          file: `/etc/yum.repos.d/${repo.name}.repo`,
          code: `[${repo.name}]
name=${repo.name} (via Trove)
baseurl=${url}/
enabled=1
gpgcheck=1${readAuth ? "\nusername=your-user\npassword=<TOKEN>" : ""}`,
        },
        { file: "shell", code: `dnf clean all && dnf makecache --repo ${repo.name}` },
      ];
    case "raw":
    default: {
      const download = `curl -O ${readAuth ? "-u your-user:<TOKEN> " : ""}${url}/${canPublish ? "releases/app.tar.gz" : "path/to/file"}`;
      return [
        {
          file: "shell",
          code: canPublish ? `curl -u your-user:<TOKEN> --upload-file ./app.tar.gz ${url}/releases/app.tar.gz\n${download}` : download,
        },
      ];
    }
  }
}
