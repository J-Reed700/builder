FROM nginx:1.28-alpine
COPY 19-builder-upstream.envsh /docker-entrypoint.d/19-builder-upstream.envsh
COPY nginx.conf.template /etc/nginx/templates/default.conf.template
COPY web/ /usr/share/nginx/html/
ENV BUILDER_UPSTREAM=host.docker.internal:7432
ENV NGINX_ENVSUBST_FILTER=BUILDER_UPSTREAM
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=3s CMD wget -q -O /dev/null http://127.0.0.1:8080/ || exit 1
