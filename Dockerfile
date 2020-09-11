FROM gradle:jdk11 as builder


COPY --chown=gradle:gradle . /home/gradle/project
WORKDIR /home/gradle/project
RUN gradle :mapsforge-map-writer:build --info --stacktrace


FROM openjdk:11-jre-slim
EXPOSE 8080
WORKDIR /app
RUN apt update && apt install -y curl && \
    curl --fail -L https://github.com/openstreetmap/osmosis/releases/download/0.48.3/osmosis-0.48.3.tgz -Oosmosis-latest.tgz && \
    tar -xvzf osmosis-0.48.3.tgz
COPY --from=builder /home/gradle/project/mapsforge-map-writer/build/libs/mapsforge-map-writer-master-SNAPSHOT-jar-with-dependencies.jar /app/plugins/
CMD /app/bin/osmosis