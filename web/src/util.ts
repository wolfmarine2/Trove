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

/** Client configuration for a repository, generated from its format and type. */
export function clientSnippets(repo: Repository): Snippet[] {
  const url = repo.url;
  const host = url.replace(/^https?:\/\//, "");
  const canPublish = repo.kind === "hosted";
  switch (repo.format) {
    case "maven": {
      const out: Snippet[] = [
        {
          file: "~/.m2/settings.xml",
          code: `<settings>
  <servers>
    <server>
      <id>${repo.name}</id>
      <username>your-user</username>
      <password><TOKEN></password>
    </server>
  </servers>
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
      if (canPublish)
        out.push({
          file: "pom.xml",
          code: `<distributionManagement>
  <${repo.config.version_policy === "snapshot" ? "snapshotRepository" : "repository"}>
    <id>${repo.name}</id>
    <url>${url}/</url>
  </${repo.config.version_policy === "snapshot" ? "snapshotRepository" : "repository"}>
</distributionManagement>`,
        });
      out.push({
        file: "build.gradle.kts",
        code: `repositories {
    maven {
        url = uri("${url}/")
        credentials { username = "your-user"; password = "<TOKEN>" }
    }
}`,
      });
      return out;
    }
    case "npm":
      return canPublish
        ? [
            {
              file: ".npmrc",
              code: `@your-scope:registry=${url}/
//${host}/:_authToken=<TOKEN>`,
            },
            { file: "shell", code: `npm publish --registry ${url}/` },
          ]
        : [
            {
              file: ".npmrc",
              code: `registry=${url}/
//${host}/:_authToken=<TOKEN>`,
            },
            { file: "shell", code: `npm install --registry ${url}/` },
          ];
    case "pypi":
      return canPublish
        ? [
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
            { file: "pip.conf", code: `[global]\nindex-url = ${url}/simple/` },
          ]
        : [
            { file: "pip.conf", code: `[global]\nindex-url = ${url}/simple/` },
            { file: "shell", code: `pip install --index-url ${url}/simple/ requests\nuv pip install --index-url ${url}/simple/ requests` },
          ];
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
        { file: "shell", code: `cargo login --registry ${repo.name} <TOKEN>` },
      ];
      if (canPublish) out.push({ file: "shell", code: `cargo publish --registry ${repo.name}` });
      return out;
    }
    case "docker": {
      const origin = url.replace(/\/repository\/.*$/, "");
      const registry = origin.replace(/^https?:\/\//, "");
      const isHub = (repo.config.proxy?.remote_url ?? "").includes("docker.io") || repo.kind === "group";
      const image = isHub ? "library/nginx:1.27" : "<image>:<tag>";
      const out: Snippet[] = [
        { file: "shell", code: `docker login ${registry}\ndocker pull ${registry}/${repo.name}/${image}` },
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
      return out;
    }
    case "apt":
      return [
        { file: "/etc/apt/sources.list.d/trove.list", code: `deb ${url}/ ${suiteHint(repo.config.proxy?.remote_url)} main` },
        {
          file: "/etc/apt/sources.list.d/trove.sources",
          code: `Types: deb
URIs: ${url}/
Suites: ${suiteHint(repo.config.proxy?.remote_url)}
Components: main
Signed-By: /usr/share/keyrings/<distribution>-archive-keyring.gpg`,
        },
      ];
    case "yum":
      return [
        {
          file: `/etc/yum.repos.d/${repo.name}.repo`,
          code: `[${repo.name}]
name=${repo.name} (via Trove)
baseurl=${url}/
enabled=1
gpgcheck=1`,
        },
        { file: "shell", code: `dnf clean all && dnf makecache --repo ${repo.name}` },
      ];
    case "raw":
    default:
      return [
        {
          file: "shell",
          code: canPublish
            ? `curl -u your-user:<TOKEN> --upload-file ./app.tar.gz ${url}/releases/app.tar.gz
curl -O ${url}/releases/app.tar.gz`
            : `curl -O ${url}/path/to/file`,
        },
      ];
  }
}
