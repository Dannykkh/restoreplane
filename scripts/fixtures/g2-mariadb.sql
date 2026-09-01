SET NAMES utf8mb4;

CREATE TABLE items (
    id INT PRIMARY KEY,
    label VARCHAR(128) NOT NULL,
    payload VARBINARY(32) NOT NULL,
    created_at TIMESTAMP NOT NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

CREATE TABLE audit_log (
    id INT AUTO_INCREMENT PRIMARY KEY,
    item_id INT NOT NULL,
    action_name VARCHAR(32) NOT NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

DELIMITER //
CREATE TRIGGER items_after_insert
AFTER INSERT ON items
FOR EACH ROW
BEGIN
    INSERT INTO audit_log(item_id, action_name) VALUES (NEW.id, 'insert');
END//

CREATE FUNCTION item_label(item_id INT)
RETURNS VARCHAR(128)
DETERMINISTIC
READS SQL DATA
BEGIN
    DECLARE result VARCHAR(128);
    SELECT label INTO result FROM items WHERE id = item_id;
    RETURN result;
END//

CREATE PROCEDURE item_count(OUT total INT)
READS SQL DATA
BEGIN
    SELECT COUNT(*) INTO total FROM items;
END//

CREATE EVENT daily_fixture_event
ON SCHEDULE EVERY 1 DAY
STARTS '2035-01-01 00:00:00'
DO
BEGIN
    INSERT INTO audit_log(item_id, action_name) VALUES (0, 'event');
END//
DELIMITER ;

CREATE VIEW item_summary AS
SELECT id, label, HEX(payload) AS payload_hex
FROM items;

INSERT INTO items(id, label, payload, created_at) VALUES
    (1, '서울', UNHEX('000102FF'), '2026-01-02 03:04:05'),
    (2, 'restore-proof', UNHEX('DEADBEEF'), '2026-02-03 04:05:06');
