FROM alpine@sha256:c0d488a800e4127c334ad20d61d7bc21b4097540327217dfab52262adc02380c
RUN touch /hello-script;\
    /bin/sh -c "echo -e '"'#!/bin/sh\nwhile true; do echo "hello"; sleep 2; done;\n'"' > /hello-script"
RUN mkdir -p /opt/evervault
ADD https://cage-build-assets.evervault.com/installer/b8073166b7c5bc8fe2abf192f66e1106f2d4be547b1841be69f95ff2c4ea578c.tar.gz /opt/evervault/runtime-dependencies.tar.gz
RUN cd /opt/evervault ; tar -xzf runtime-dependencies.tar.gz ; sh ./installer.sh ; rm runtime-dependencies.tar.gz
RUN mkdir -p /etc/service/user-entrypoint
RUN printf "#!/bin/sh\nsleep 5\necho \"Checking status of data-plane\"\nSVDIR=/etc/service sv check data-plane || exit 1\necho \"Data-plane up and running\"\nwhile ! grep -q \"EV_CAGE_INITIALIZED\" /etc/customer-env\n do echo \"Env not ready, sleeping user process for one second\"\n sleep 1\n done \n . /etc/customer-env\n\necho \"Booting user service...\"\ncd %s\nexec sh /hello-script\n" "$PWD"  > /etc/service/user-entrypoint/run && chmod +x /etc/service/user-entrypoint/run
ADD https://cage-build-assets.evervault.com/runtime/0.0.29/data-plane/egress-disabled/tls-termination-enabled /opt/evervault/data-plane
RUN chmod +x /opt/evervault/data-plane
RUN mkdir -p /etc/service/data-plane
RUN printf "#!/bin/sh\necho \"Booting Evervault data plane...\"\nexec /opt/evervault/data-plane 80\n" > /etc/service/data-plane/run && chmod +x /etc/service/data-plane/run
RUN printf "#!/bin/sh\nifconfig lo 127.0.0.1\n echo \"enclave.local\" > /etc/hostname \n echo \"127.0.0.1 enclave.local\" >> /etc/hosts \n hostname -F /etc/hostname \necho \"Booting enclave...\"\nexec runsvdir /etc/service\n" > /bootstrap && chmod +x /bootstrap
RUN find $( ls / | grep -E -v "^(dev|mnt|proc|sys)$" ) -xdev | xargs touch --date="@0" --no-dereference || true
FROM scratch
COPY --from=0 / /
ENTRYPOINT ["/bootstrap", "1>&2"]
