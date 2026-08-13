# TossIt 官网

这是一个零依赖静态站点，直接用浏览器打开 `index.html`，或在本目录启动静态服务器即可预览。

```bash
python3 -m http.server 4173 --bind 127.0.0.1 --directory website
```

生产环境使用独立站点目录 `/var/www/tossit.mlxb.cc`。每次发布写入新的 `releases/<时间戳>`，再把 `current` 软链接切到新版本，不改动服务器上其他项目。
