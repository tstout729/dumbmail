-- A metric an integration recipe declares as a purchase or a stay (`"purchase": true` on its entry in
-- recipe.json's metrics). A review asked about such an event counts as verified.
ALTER TABLE metrics ADD COLUMN purchase boolean NOT NULL DEFAULT false;

-- Which of a catalog product's address and image a browser supplied (a product learned from browser
-- events), field by field. A trusted source that later updates the product without sending that
-- field leaves it marked; one that sends a different value clears the mark. The reviews widget and
-- emails show only unmarked addresses and images.
ALTER TABLE catalog_products
    ADD COLUMN url_from_browser boolean NOT NULL DEFAULT false,
    ADD COLUMN image_from_browser boolean NOT NULL DEFAULT false;
UPDATE catalog_products SET url_from_browser = url IS NOT NULL, image_from_browser = image_url IS NOT NULL WHERE source = 'events';

CREATE FUNCTION catalog_products_field_origin() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.source = 'events' THEN
        NEW.url_from_browser := NEW.url IS NOT NULL;
        NEW.image_from_browser := NEW.image_url IS NOT NULL;
    ELSIF TG_OP = 'UPDATE' THEN
        NEW.url_from_browser := OLD.url_from_browser AND NEW.url IS NOT DISTINCT FROM OLD.url;
        NEW.image_from_browser := OLD.image_from_browser AND NEW.image_url IS NOT DISTINCT FROM OLD.image_url;
    ELSE
        NEW.url_from_browser := false;
        NEW.image_from_browser := false;
    END IF;
    RETURN NEW;
END $$;

CREATE TRIGGER catalog_products_field_origin BEFORE INSERT OR UPDATE OF url, image_url, source ON catalog_products
    FOR EACH ROW EXECUTE FUNCTION catalog_products_field_origin();
