FROM gradle:jdk21-corretto AS builder

COPY --chown=gradle:gradle . /home/gradle/project
WORKDIR /home/gradle/project
RUN gradle :mapsforge-map-writer:fatJar --info --stacktrace


FROM amazoncorretto:21
EXPOSE 8080
WORKDIR /app
RUN yum install -y tar && curl --fail -L https://github.com/openstreetmap/osmosis/releases/download/0.49.2/osmosis-0.49.2.tar -o osmosis-latest.tar && \
    tar -xvf osmosis-latest.tar --strip-components=1
COPY --from=builder /home/gradle/project/mapsforge-map-writer/build/libs/mapsforge-map-writer-master-SNAPSHOT-jar-with-dependencies.jar /app/plugins/
CMD /app/bin/osmosis