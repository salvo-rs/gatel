Name:           gatel
Version:        {{VERSION}}
Release:        1%{?dist}
Summary:        High-performance reverse proxy and web server
License:        Apache-2.0
URL:            https://github.com/salvo-rs/gatel
Source0:        gatel-%{version}.tar.gz

%description
Gatel is a modern reverse proxy and web server built with Rust,
inspired by Caddy. It uses KDL as its configuration language and
supports automatic HTTPS, load balancing, compression, rate
limiting, and more.

%prep
%setup -q

%install
install -Dm755 gatel %{buildroot}/usr/local/bin/gatel
install -Dm644 gatel.service %{buildroot}/lib/systemd/system/gatel.service
install -dm750 %{buildroot}/etc/gatel
install -dm755 %{buildroot}/var/log/gatel
install -dm755 %{buildroot}/var/lib/gatel

cat > %{buildroot}/etc/gatel/gatel.kdl <<'EOF'
global {
    log level="info"
    http ":80"
}

site "*" {
    route "/*" {
        respond "Hello from gatel!" status=200
    }
}
EOF

%pre
getent group gatel >/dev/null || groupadd -r gatel
getent passwd gatel >/dev/null || \
    useradd -r -g gatel -d /var/lib/gatel -s /sbin/nologin \
    -c "Gatel reverse proxy" gatel
exit 0

%post
%systemd_post gatel.service
chown -R gatel:gatel /var/log/gatel /var/lib/gatel
chown root:gatel /etc/gatel
chmod 750 /etc/gatel

%preun
%systemd_preun gatel.service

%postun
%systemd_postun_with_restart gatel.service

%files
%attr(755, root, root) /usr/local/bin/gatel
%attr(644, root, root) /lib/systemd/system/gatel.service
%config(noreplace) %attr(640, root, gatel) /etc/gatel/gatel.kdl
%dir %attr(750, root, gatel) /etc/gatel
%dir %attr(755, gatel, gatel) /var/log/gatel
%dir %attr(755, gatel, gatel) /var/lib/gatel

%changelog
