FROM gradle:jdk8-alpine as builder


COPY --chown=gradle:gradle . /home/gradle/project
WORKDIR /home/gradle/project
RUN gradle :mapsforge-map-writer:build --info --stacktrace


FROM openjdk:8-jre-alpine
EXPOSE 8080
WORKDIR /app
RUN apk update && apk add curl && \
    curl --fail -O https://bretth.dev.openstreetmap.org/osmosis-build/osmosis-latest.tgz -Oosmosis-latest.tgz && \
    tar -xvzf osmosis-latest.tgz
COPY --from=builder /home/gradle/project/mapsforge-map-writer/build/libs/mapsforge-map-writer-master-SNAPSHOT-jar-with-dependencies.jar /app/plugins/
CMD /app/bin/osmosis