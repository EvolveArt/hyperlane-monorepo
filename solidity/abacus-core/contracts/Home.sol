// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity >=0.6.11;

// ============ Internal Imports ============
import {Version0} from "./Version0.sol";
import {Common} from "./Common.sol";
import {MerkleLib} from "../libs/Merkle.sol";
import {Message} from "../libs/Message.sol";
import {MerkleTreeManager} from "./Merkle.sol";

/**
 * @title Home
 * @author Celo Labs Inc.
 * @notice Accepts messages to be dispatched to remote chains,
 * constructs a Merkle tree of the messages,
 * and accepts signatures from a bonded Updater
 * which notarize the Merkle tree roots.
 * Accepts submissions of fraudulent signatures
 * by the Updater and slashes the Updater in this case.
 */
contract Home is
    Version0,
    MerkleTreeManager,
    Common
{
    // ============ Libraries ============

    using MerkleLib for MerkleLib.Tree;

    // ============ Constants ============

    // Maximum bytes per message = 2 KiB
    // (somewhat arbitrarily set to begin)
    uint256 public constant MAX_MESSAGE_BODY_BYTES = 2 * 2**10;

    // ============ Enums ============

    // States:
    //   0 - UnInitialized - before initialize function is called
    //   note: the contract is initialized at deploy time, so it should never be in this state
    //   1 - Active - as long as the contract has not become fraudulent
    //   2 - Failed - after a valid fraud proof has been submitted;
    //   contract will no longer accept updates or new messages
    enum States {
        UnInitialized,
        Active,
        Failed
    }

    // ============ Public Storage Variables ============

    // Current state of contract
    States public state;
    // Checkpoints of root => leaf index
    mapping(bytes32 => uint256) public checkpoints;
    // domain => next available nonce for the domain
    mapping(uint32 => uint32) public nonces;
    // The latest checkpointed root
    bytes32 checkpointedRoot;

    // ============ Upgrade Gap ============

    // gap for upgrade safety
    uint256[46] private __GAP;

    // ============ Events ============

    /**
     * @notice Emitted when a new message is dispatched via Abacus
     * @param leafIndex Index of message's leaf in merkle tree
     * @param destinationAndNonce Destination and destination-specific
     * nonce combined in single field ((destination << 32) & nonce)
     * @param messageHash Hash of message; the leaf inserted to the Merkle tree for the message
     * @param checkpointedRoot the latest notarized root submitted in the last signed Update
     * @param message Raw bytes of message
     */
    event Dispatch(
        bytes32 indexed messageHash,
        uint256 indexed leafIndex,
        uint64 indexed destinationAndNonce,
        bytes32 checkpointedRoot,
        bytes message
    );

    // ============ Constructor ============

    constructor(uint32 _localDomain) Common(_localDomain) {} // solhint-disable-line no-empty-blocks

    // ============ Initializer ============

    function initialize(address _updaterManager) public initializer {
        __Common_initialize(_updaterManager);
        state = States.Active;
    }

    // ============ Modifiers ============

    /**
     * @notice Ensures that contract state != FAILED when the function is called
     */
    modifier notFailed() {
        require(state != States.Failed, "failed state");
        _;
    }

    /**
     * @notice Ensures that function is called by the UpdaterManager contract
     */
    modifier onlyUpdaterManager() {
        require(msg.sender == address(updaterManager), "!updaterManager");
        _;
    }

    // ============ External Functions  ============

    /**
     * @notice Dispatch the message it to the destination domain & recipient
     * @dev Format the message, insert its hash into Merkle tree,
     * and emit `Dispatch` event with message information.
     * @param _destinationDomain Domain of destination chain
     * @param _recipientAddress Address of recipient on destination chain as bytes32
     * @param _messageBody Raw bytes content of message
     */
    function dispatch(
        uint32 _destinationDomain,
        bytes32 _recipientAddress,
        bytes memory _messageBody
    ) external notFailed {
        require(_messageBody.length <= MAX_MESSAGE_BODY_BYTES, "msg too long");
        // get the next nonce for the destination domain, then increment it
        uint32 _nonce = nonces[_destinationDomain];
        nonces[_destinationDomain] = _nonce + 1;
        // format the message into packed bytes
        bytes memory _message = Message.formatMessage(
            localDomain,
            bytes32(uint256(uint160(msg.sender))),
            _nonce,
            _destinationDomain,
            _recipientAddress,
            _messageBody
        );
        // insert the hashed message into the Merkle tree
        bytes32 _messageHash = keccak256(_message);
        tree.insert(_messageHash);
        // Emit Dispatch event with message information
        // note: leafIndex is count() - 1 since new leaf has already been inserted
        emit Dispatch(
            _messageHash,
            count() - 1,
            _destinationAndNonce(_destinationDomain, _nonce),
            checkpointedRoot,
            _message
        );
    }

    /**
     * @notice Checkpoints the latest root and index.
     * Updaters are expected to sign this checkpoint so that it can be
     * relayed to the Replica contracts.
     * @dev emits Checkpoint event
     */
    function checkpoint() external notFailed {
        uint256 count = count();
        require(count > 0, "!count");
        uint256 index = count - 1;
        bytes32 root = root();
        checkpointedRoot = root;
        checkpoints[root] = index;
        emit Checkpoint(root, index);
    }

    /**
     * @notice Set contract state to FAILED.
     * @dev Called by the UpdaterManager when fraud is proven.
     */
    function fail() external onlyUpdaterManager {
        // set contract to FAILED
        state = States.Failed;
    }

    /**
     * @notice Returns the latest checkpoint for the Updaters to sign.
     * @return root Latest checkpointed root
     * @return index Latest checkpointed index
     */
    function latestCheckpoint()
        external
        view
        returns (bytes32 root, uint256 index)
    {
        root = checkpointedRoot;
        index = checkpoints[root];
    }

    // ============ Internal Functions  ============

    /**
     * @notice Internal utility function that combines
     * `_destination` and `_nonce`.
     * @dev Both destination and nonce should be less than 2^32 - 1
     * @param _destination Domain of destination chain
     * @param _nonce Current nonce for given destination chain
     * @return Returns (`_destination` << 32) & `_nonce`
     */
    function _destinationAndNonce(uint32 _destination, uint32 _nonce)
        internal
        pure
        returns (uint64)
    {
        return (uint64(_destination) << 32) | _nonce;
    }
}
