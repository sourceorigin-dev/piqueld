-- The images a build machine pushed to the build registry for this installation,
-- as `repository@sha256:<digest>`. Image cleanup deletes from the registry
-- those no retained deployment uses, then forgets them. Build records go
-- with their environment and deployment history, so a pushed image is
-- recorded here until it is deleted from the registry, and none is left
-- behind there. The images earlier builds pushed are recorded too: those of
-- builds that built rather than reused one, and not job runs, which record
-- the image they ran.
CREATE TABLE pushed_images (
    image TEXT PRIMARY KEY
) STRICT;

INSERT OR IGNORE INTO pushed_images(image)
SELECT image_id FROM builds
WHERE image_id LIKE '%@sha256:%' AND job IS NULL AND reused_build_id IS NULL;
